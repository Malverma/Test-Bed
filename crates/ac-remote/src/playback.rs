//! The viewer's desktop audio: Opus packets decoded and played through
//! `pw-cat`, like phone audio in Test Bed. (RemoteLinux played it with SDL.)
//!
//! The queue stays short so audio keeps in step with the picture: packets
//! that arrive while the player is behind are dropped, and Opus conceals a
//! few lost packets in a row.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::proto::{AUDIO_CHANNELS, AUDIO_FRAME, AUDIO_RATE};

/// Conceal at most this many lost packets in a row.
const MAX_CONCEAL: u32 = 5;
/// Packets waiting for the player beyond this are dropped (about 100 ms).
const MAX_QUEUED: usize = 10;

/// Starts the player. Send `(sequence, packet)`; dropping the sender stops it.
pub fn spawn(description: &str) -> Result<mpsc::SyncSender<(u32, Vec<u8>)>> {
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
        play(rx, &mut dec, &mut stdin);
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
    })?;
    Ok(tx)
}

fn play(rx: mpsc::Receiver<(u32, Vec<u8>)>, dec: &mut opus::Decoder, stdin: &mut impl Write) {
    {
        let mut pcm = vec![0i16; AUDIO_FRAME * AUDIO_CHANNELS * 6];
        let mut next: Option<u32> = None;
        let mut write = |pcm: &[i16]| {
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            stdin.write_all(&bytes).is_ok()
        };
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
}
