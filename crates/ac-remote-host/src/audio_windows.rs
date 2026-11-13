//! Desktop audio on Windows: WASAPI loopback of the default output (cpal
//! records an output device in loopback mode), resampled to 48 kHz stereo
//! and encoded as Opus, one 10 ms packet at a time.

use std::sync::mpsc as std_mpsc;

use anyhow::{anyhow, Context, Result};
use ac_remote::proto::{AUDIO_CHANNELS, AUDIO_FRAME, AUDIO_RATE};
use ac_remote::resample::Resampler;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tokio::sync::mpsc;

/// Start capturing on a thread. Packets go to `tx`; the thread exits when `tx`
/// is closed. A failure is sent once on `err` (audio is optional, so it never
/// ends the session).
pub fn spawn(tx: mpsc::UnboundedSender<Vec<u8>>, err: mpsc::UnboundedSender<String>) {
    std::thread::Builder::new()
        .name("share-audio".into())
        .spawn(move || {
            if let Err(e) = run(&tx) {
                if !tx.is_closed() {
                    tracing::warn!("audio: {e:#}");
                    let _ = err.send(format!("{e:#}"));
                }
            }
        })
        .expect("spawning audio thread");
}

/// Interleaved samples at the device's rate, folded to stereo.
fn to_stereo(data: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(data.iter().flat_map(|&s| [s, s])),
        _ => out.extend(data.chunks_exact(channels).flat_map(|f| [f[0], f[1]])),
    }
}

fn run(tx: &mpsc::UnboundedSender<Vec<u8>>) -> Result<()> {
    let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no sound output on this PC"))?;
    let supported = device.default_output_config().context("reading the sound output's format")?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let (rate, channels) = (config.sample_rate, config.channels as usize);
    let (raw_tx, raw_rx) = std_mpsc::sync_channel::<Vec<f32>>(64);
    let on_err = |e| tracing::warn!("audio capture: {e}");
    let stream = match format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |d: &[f32], _: &_| {
                let _ = raw_tx.try_send(d.to_vec());
            },
            on_err,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |d: &[i16], _: &_| {
                let _ = raw_tx.try_send(d.iter().map(|&s| s as f32 / 32768.0).collect());
            },
            on_err,
            None,
        ),
        f => return Err(anyhow!("unsupported sound format {f:?}")),
    }
    .context("recording the sound output")?;
    stream.play().context("starting audio capture")?;

    let mut enc = opus::Encoder::new(AUDIO_RATE, opus::Channels::Stereo, opus::Application::LowDelay)?;
    enc.set_bitrate(opus::Bitrate::Bits(128_000))?;
    let mut rs = Resampler::new(rate, AUDIO_RATE);
    let (mut stereo, mut pcm) = (Vec::new(), Vec::new());
    let mut packet = [0u8; 1500];
    let frame = AUDIO_FRAME * AUDIO_CHANNELS;
    tracing::info!("audio capture started ({rate} Hz, {channels} channels)");
    while !tx.is_closed() {
        // Windows sends nothing while nothing plays; check `tx` now and then.
        let Ok(chunk) = raw_rx.recv_timeout(std::time::Duration::from_millis(200)) else { continue };
        stereo.clear();
        to_stereo(&chunk, channels, &mut stereo);
        rs.push(&stereo, &mut pcm);
        while pcm.len() >= frame {
            let n = enc.encode_float(&pcm[..frame], &mut packet)?;
            pcm.drain(..frame);
            if tx.send(packet[..n].to_vec()).is_err() {
                return Ok(());
            }
        }
    }
    drop(stream);
    Ok(())
}
