//! The viewer's desktop audio: Opus packets decoded and played through
//! `pw-cat` on Linux, like phone audio in Test Bed, and WASAPI (cpal) on
//! Windows.
//!
//! The queue stays short so audio keeps in step with the picture: packets
//! that arrive while the player is behind are dropped, and Opus conceals a
//! few lost packets in a row.

use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::proto::{AUDIO_CHANNELS, AUDIO_FRAME, AUDIO_RATE};

/// Conceal at most this many lost packets in a row.
const MAX_CONCEAL: u32 = 5;
/// Packets waiting for the player beyond this are dropped (about 100 ms).
const MAX_QUEUED: usize = 10;

/// Starts the player. Send `(sequence, packet)`; dropping the sender stops it.
#[cfg(unix)]
pub fn spawn(description: &str) -> Result<mpsc::SyncSender<(u32, Vec<u8>)>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let desc: String = description.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect();
    let mut child = Command::new("pw-cat")
        .args(["--playback", "--raw", "--format", "s16", "--rate", &AUDIO_RATE.to_string(), "--channels", &AUDIO_CHANNELS.to_string()])
        .args(["--latency", "1024", "--media-role", "Game", "--properties"])
        .arg(format!("{{ node.name = \"test-bed.remote\" node.description = \"{desc}\" application.name = \"Test Bed\" }}"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting pw-cat (it comes with PipeWire: install pipewire-audio)")?;
    let mut stdin = child.stdin.take().context("pw-cat has no input")?;
    let mut dec = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo)?;
    let (tx, rx) = mpsc::sync_channel::<(u32, Vec<u8>)>(MAX_QUEUED);
    std::thread::Builder::new().name("remote-audio".into()).spawn(move || {
        play(rx, &mut dec, |pcm| {
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            stdin.write_all(&bytes).is_ok()
        });
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
    })?;
    Ok(tx)
}

/// Starts the player. Send `(sequence, packet)`; dropping the sender stops it.
#[cfg(windows)]
pub fn spawn(_description: &str) -> Result<mpsc::SyncSender<(u32, Vec<u8>)>> {
    let mut dec = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo)?;
    let (tx, rx) = mpsc::sync_channel::<(u32, Vec<u8>)>(MAX_QUEUED);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(1);
    // cpal's stream has to stay on the thread that made it.
    std::thread::Builder::new().name("remote-audio".into()).spawn(move || {
        let (stream, queue, rate) = match wasapi::open() {
            Ok(opened) => {
                let _ = ready_tx.send(Ok(()));
                opened
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let mut rs = crate::resample::Resampler::new(AUDIO_RATE, rate);
        let (mut stereo, mut out) = (Vec::new(), Vec::new());
        play(rx, &mut dec, |pcm| {
            stereo.clear();
            stereo.extend(pcm.iter().map(|&s| s as f32 / 32768.0));
            out.clear();
            rs.push(&stereo, &mut out);
            wasapi::queue(&queue, &out, rate);
            true
        });
        drop(stream);
    })?;
    ready_rx.recv().context("the audio thread stopped")??;
    Ok(tx)
}

#[cfg(windows)]
mod wasapi {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use anyhow::{anyhow, Context, Result};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};

    /// Interleaved stereo at the device's rate, waiting for the device.
    pub type Queue = Arc<Mutex<VecDeque<f32>>>;

    /// More than this much waiting (seconds) and the oldest is dropped, so
    /// sound stays in step with the picture.
    const MAX_BUFFERED: f32 = 0.15;

    /// Opens the default output; returns the stream, its queue and its rate.
    pub fn open() -> Result<(cpal::Stream, Queue, u32)> {
        let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no sound output on this PC"))?;
        let supported = device.default_output_config().context("reading the sound output's format")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let (rate, channels) = (config.sample_rate, config.channels as usize);
        let queue: Queue = Arc::default();
        let on_err = |e| tracing::warn!("audio playback: {e}");
        let q = queue.clone();
        let stream = match format {
            SampleFormat::F32 => device.build_output_stream(config, move |d: &mut [f32], _: &_| fill(&q, d, channels, |s| s), on_err, None),
            SampleFormat::I16 => device.build_output_stream(
                config,
                move |d: &mut [i16], _: &_| fill(&q, d, channels, |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
                on_err,
                None,
            ),
            f => return Err(anyhow!("unsupported sound format {f:?}")),
        }
        .context("opening the sound output")?;
        stream.play().context("starting audio playback")?;
        Ok((stream, queue, rate))
    }

    /// Device callback: stereo from the queue into however many channels the
    /// device has; silence when the queue runs dry.
    fn fill<T: Copy>(queue: &Queue, out: &mut [T], channels: usize, conv: impl Fn(f32) -> T) {
        let mut q = queue.lock().unwrap();
        for frame in out.chunks_mut(channels.max(1)) {
            let (l, r) = match (q.pop_front(), q.pop_front()) {
                (Some(l), Some(r)) => (l, r),
                _ => (0.0, 0.0),
            };
            match frame {
                [m] => *m = conv((l + r) * 0.5),
                [a, b, rest @ ..] => {
                    *a = conv(l);
                    *b = conv(r);
                    rest.iter_mut().for_each(|s| *s = conv(0.0));
                }
                [] => {}
            }
        }
    }

    pub fn queue(queue: &Queue, samples: &[f32], rate: u32) {
        let mut q = queue.lock().unwrap();
        q.extend(samples);
        let max = (rate as f32 * MAX_BUFFERED) as usize * 2;
        if q.len() > max {
            let extra = q.len() - max;
            q.drain(..extra);
        }
    }
}

fn play(rx: mpsc::Receiver<(u32, Vec<u8>)>, dec: &mut opus::Decoder, mut write: impl FnMut(&[i16]) -> bool) {
    let mut pcm = vec![0i16; AUDIO_FRAME * AUDIO_CHANNELS * 6];
    let mut next: Option<u32> = None;
    for (seq, packet) in rx {
        match next.map(|n| seq.wrapping_sub(n) as i32) {
            Some(g) if (-50..0).contains(&g) => continue, // late or duplicate
            Some(g) if (1..50).contains(&g) => {
                for _ in 0..(g as u32).min(MAX_CONCEAL) {
                    // Loss concealment: the buffer length sets the frame size.
                    if let Ok(k) = dec.decode(&[], &mut pcm[..AUDIO_FRAME * AUDIO_CHANNELS], false) {
                        if !write(&pcm[..k * AUDIO_CHANNELS]) {
                            return;
                        }
                    }
                }
            }
            _ => {} // first packet, or the host restarted the stream
        }
        next = Some(seq.wrapping_add(1));
        match dec.decode(&packet, &mut pcm, false) {
            Ok(k) => {
                if !write(&pcm[..k * AUDIO_CHANNELS]) {
                    return;
                }
            }
            Err(e) => tracing::debug!("audio decode: {e}"),
        }
    }
}
