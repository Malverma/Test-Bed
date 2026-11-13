//! The viewer's connection to a shared PC (from RemoteLinux's `rl-client`):
//! session setup, video receive, input send,
//! keepalive pings and automatic reconnection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crate::identity::Identity;
use crate::net::Conn;
use crate::proto::{read_msg, write_msg, ClientMsg, HostMsg, MonitorInfo, Motion, StreamSettings};
use crate::transport::{self, ALPN_SESSION};
use crate::trust::{KnownHost, KnownHosts};
use crate::video::{FrameMeta, Reassembler};
use tokio::sync::mpsc;

/// QUIC close codes (must match the host).
mod close {
    pub const NOT_PAIRED: u32 = 1;
    pub const REPLACED: u32 = 2;
    pub const VERSION: u32 = 3;
    pub const SHUTDOWN: u32 = 5;
}

const PING_INTERVAL: Duration = Duration::from_millis(250);
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// An incomplete frame with no new fragments for this long is considered lost.
const STALE_PARTIAL: Duration = Duration::from_millis(60);

/// Events from the network side to the viewer.
pub enum NetEvent {
    Connecting { attempt: u32 },
    Connected { host_name: String, monitors: Vec<MonitorInfo> },
    Stream { monitor: String, width: u32, height: u32, fps: u32 },
    Frame { meta: FrameMeta, data: Vec<u8> },
    /// One Opus packet of desktop audio.
    Audio { seq: u32, data: Vec<u8> },
    /// Latest clock sync: RTT and host-minus-client clock offset.
    Clock { rtt_us: u64, offset_us: i64 },
    FramesDropped(u32),
    Notice(String),
    Disconnected { reason: String },
    Reconnecting { retry_in: Duration, attempt: u32, reason: String },
    /// The host agreed to switch sides; this is the viewer it will connect back with.
    SwitchReady { client_name: String, client_fingerprint: String },
    SwitchRefused(String),
    /// A file going to the host: `sent` of `total` bytes so far.
    FileProgress { name: String, sent: u64, total: u64 },
    /// The host saved it at `path` (a path on the host).
    FileSent { name: String, path: String },
    FileFailed { name: String, error: String },
    /// Unrecoverable: the viewer should show it and stop.
    Fatal(String),
}

/// Input and control from the viewer to the network side.
pub enum Outgoing {
    Msg(ClientMsg),
    Motion(Motion),
    /// Send a file; `drop_at` is where on the stream it was dropped (0..1).
    SendFile { path: std::path::PathBuf, drop_at: Option<(f32, f32)> },
    Quit,
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

pub struct NetHandle {
    pub tx: mpsc::UnboundedSender<Outgoing>,
    pub quit: Arc<AtomicBool>,
}

enum Ended {
    Quit,
    Fatal(String),
    Retry(String),
}

pub fn spawn(host: KnownHost, settings: StreamSettings, client_name: String, events: impl Fn(NetEvent) + Send + Sync + 'static) -> Result<NetHandle> {
    let identity = Identity::load_or_create(crate::identity::Role::Client)?;
    let (tx, rx) = mpsc::unbounded_channel();
    let quit = Arc::new(AtomicBool::new(false));
    let q = quit.clone();
    let req_tx = tx.clone();
    std::thread::Builder::new().name("remote-net".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        rt.block_on(run(host, settings, client_name, identity, rx, req_tx, q, Arc::new(events)));
    })?;
    Ok(NetHandle { tx, quit })
}

#[allow(clippy::too_many_arguments)]
async fn run(
    host: KnownHost,
    mut settings: StreamSettings,
    client_name: String,
    identity: Identity,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    req_tx: mpsc::UnboundedSender<Outgoing>,
    quit: Arc<AtomicBool>,
    events: Arc<dyn Fn(NetEvent) + Send + Sync>,
) {
    let endpoint = match Dialer::new(&host).await {
        Ok(e) => e,
        Err(e) => return events(NetEvent::Fatal(format!("{e:#}"))),
    };
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        events(NetEvent::Connecting { attempt });
        let started = Instant::now();
        let (res, connected) = session(&endpoint, &host, &mut settings, &client_name, &identity, &mut rx, &req_tx, &events).await;
        if connected && started.elapsed() > Duration::from_secs(5) {
            attempt = 1; // a session that got going resets the backoff
        }
        if quit.load(Ordering::Relaxed) {
            break;
        }
        let reason = match res {
            Ended::Quit => break,
            Ended::Fatal(e) => {
                events(NetEvent::Fatal(e));
                break;
            }
            Ended::Retry(r) => r,
        };
        let retry_in = Duration::from_millis((250u64 << attempt.min(5)).min(5000));
        events(NetEvent::Reconnecting { retry_in, attempt, reason });
        // Wait, but keep draining viewer input (and notice quit).
        let sleep = tokio::time::sleep(retry_in);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                m = rx.recv() => match m {
                    None | Some(Outgoing::Quit) => { endpoint.close().await; return; }
                    Some(Outgoing::Msg(ClientMsg::UpdateSettings(s))) => settings = s,
                    Some(Outgoing::SendFile { path, .. }) => {
                        events(NetEvent::FileFailed { name: file_name(&path), error: "not connected".into() });
                    }
                    Some(_) => {}
                }
            }
        }
    }
    endpoint.close().await;
}

/// Where sessions are dialed from: quinn for a LAN host, iroh for one paired
/// over the internet.
enum Dialer {
    Lan(quinn::Endpoint),
    Internet(iroh::Endpoint),
}

impl Dialer {
    async fn new(host: &KnownHost) -> Result<Self> {
        if host.internet {
            let relay = crate::config::ClientConfig::load().map(|c| c.relay).unwrap_or_default();
            Ok(Dialer::Internet(crate::internet::client_endpoint(&relay).await?))
        } else {
            Ok(Dialer::Lan(transport::client_endpoint()?))
        }
    }

    async fn close(&self) {
        match self {
            Dialer::Lan(e) => {
                e.close(0u32.into(), b"bye");
                e.wait_idle().await;
            }
            Dialer::Internet(e) => e.close().await,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    endpoint: &Dialer,
    host: &KnownHost,
    settings: &mut StreamSettings,
    client_name: &str,
    identity: &Identity,
    rx: &mut mpsc::UnboundedReceiver<Outgoing>,
    req_tx: &mpsc::UnboundedSender<Outgoing>,
    events: &Arc<dyn Fn(NetEvent) + Send + Sync>,
) -> (Ended, bool) {
    let conn = match connect(endpoint, host, identity).await {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{e:#}");
            return (if msg.contains("fingerprint") { Ended::Fatal(msg) } else { Ended::Retry(msg) }, false);
        }
    };
    let r = run_session(&conn, settings, client_name, rx, req_tx, events).await;
    let ended = match r {
        Ok(e) => e,
        Err(e) => classify(&conn, e),
    };
    if !matches!(ended, Ended::Retry(_)) || !conn.is_closed() {
        conn.close(0, b"bye");
    }
    if let Ended::Retry(reason) = &ended {
        events(NetEvent::Disconnected { reason: reason.clone() });
    }
    (ended, true)
}

fn classify(conn: &Conn, e: anyhow::Error) -> Ended {
    if let Some(c) = conn.app_close() {
        let (code, reason) = (c.code, c.reason);
        return match code {
            close::NOT_PAIRED => Ended::Fatal(format!("the host does not recognise this laptop ({reason}); pair again from Test Bed's PC tab")),
            close::VERSION => Ended::Fatal(format!("incompatible host version ({reason})")),
            close::REPLACED => Ended::Fatal("another connection to the host replaced this one".into()),
            close::SHUTDOWN => Ended::Retry("host shut down".into()),
            _ => Ended::Retry(if reason.is_empty() { format!("closed by host (code {code})") } else { reason }),
        };
    }
    Ended::Retry(format!("{e:#}"))
}

async fn connect(endpoint: &Dialer, host: &KnownHost, identity: &Identity) -> Result<Conn> {
    let endpoint = match endpoint {
        Dialer::Lan(e) => e,
        // The endpoint id is checked by iroh's handshake: no fingerprint to compare.
        Dialer::Internet(e) => return crate::internet::connect(e, host, ALPN_SESSION).await,
    };
    let addr = transport::resolve(&host.address)?;
    let cfg = transport::client_config(identity, ALPN_SESSION)?;
    let conn = tokio::time::timeout(Duration::from_secs(3), endpoint.connect_with(cfg, addr, "remotelinux")?)
        .await
        .map_err(|_| anyhow!("host {} not reachable", host.address))?
        .with_context(|| format!("connecting to {}", host.address))?;
    let fp = transport::peer_fingerprint(&conn)?;
    if fp != host.fingerprint {
        conn.close(0u32.into(), b"fingerprint mismatch");
        bail!(
            "host at {} presented a different fingerprint than when paired (expected {}, got {}); if the desktop was reinstalled, pair again",
            host.address,
            crate::identity::short_fingerprint(&host.fingerprint),
            crate::identity::short_fingerprint(&fp)
        );
    }
    Ok(conn.into())
}

async fn run_session(
    conn: &Conn,
    settings: &mut StreamSettings,
    client_name: &str,
    rx: &mut mpsc::UnboundedReceiver<Outgoing>,
    req_tx: &mpsc::UnboundedSender<Outgoing>,
    events: &Arc<dyn Fn(NetEvent) + Send + Sync>,
) -> Result<Ended> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &ClientMsg::Hello { version: crate::PROTOCOL_VERSION, client_name: client_name.into(), settings: settings.clone() }).await?;
    match tokio::time::timeout(Duration::from_secs(3), read_msg::<HostMsg>(&mut recv)).await.map_err(|_| anyhow!("host did not answer"))?? {
        Some(HostMsg::Welcome { host_name, monitors, .. }) => events(NetEvent::Connected { host_name, monitors }),
        Some(HostMsg::Error { text }) => return Ok(Ended::Fatal(text)),
        other => bail!("unexpected reply {other:?}"),
    }
    let known = if matches!(conn, Conn::Internet(_)) { KnownHosts::load_internet() } else { KnownHosts::load() };
    if let Ok(mut kh) = known {
        if let Ok(fp) = conn.peer_key() {
            kh.touch(&fp);
            let _ = kh.save();
        }
    }

    // Host → client control messages.
    let control = {
        let events = events.clone();
        async move {
            let mut best_rtt = u64::MAX;
            let mut best_at = Instant::now();
            while let Some(m) = read_msg::<HostMsg>(&mut recv).await? {
                match m {
                    HostMsg::StreamInfo { monitor, width, height, fps } => events(NetEvent::Stream { monitor, width, height, fps }),
                    HostMsg::Pong { t_client_us, t_host_us } => {
                        let now = crate::clock::now_us();
                        let rtt = now.saturating_sub(t_client_us);
                        // Prefer the lowest-RTT sample of the last few seconds.
                        if rtt <= best_rtt || best_at.elapsed() > Duration::from_secs(5) {
                            best_rtt = rtt;
                            best_at = Instant::now();
                            let offset = t_host_us as i64 - (t_client_us + rtt / 2) as i64;
                            events(NetEvent::Clock { rtt_us: rtt, offset_us: offset });
                        } else {
                            events(NetEvent::Clock { rtt_us: rtt, offset_us: i64::MIN });
                        }
                    }
                    HostMsg::Notice { text } => events(NetEvent::Notice(text)),
                    HostMsg::Error { text } => bail!("host error: {text}"),
                    HostMsg::Welcome { .. } => {}
                    HostMsg::SwitchReady { client_name, client_fingerprint } => events(NetEvent::SwitchReady { client_name, client_fingerprint }),
                    HostMsg::SwitchRefused { text } => events(NetEvent::SwitchRefused(text)),
                }
            }
            anyhow::Ok(Ended::Retry("host closed the session".into()))
        }
    };

    // Video and audio datagrams.
    let video = {
        let events = events.clone();
        let conn = conn.clone();
        let req_tx = req_tx.clone();
        async move {
            let mut ra = Reassembler::new();
            let mut last_fragment = Instant::now();
            let mut last_key_req = Instant::now() - KEYFRAME_MIN_INTERVAL;
            let request_key = |last: &mut Instant| {
                if last.elapsed() >= KEYFRAME_MIN_INTERVAL {
                    *last = Instant::now();
                    let _ = req_tx.send(Outgoing::Msg(ClientMsg::RequestKeyframe));
                }
            };
            loop {
                let d = match tokio::time::timeout(STALE_PARTIAL, conn.read_datagram()).await {
                    Ok(d) => d?,
                    Err(_) => {
                        if ra.pending() && last_fragment.elapsed() >= STALE_PARTIAL {
                            ra.drop_pending();
                            events(NetEvent::FramesDropped(1));
                            request_key(&mut last_key_req);
                        }
                        continue;
                    }
                };
                if let Some((seq, packet)) = crate::proto::decode_audio(&d) {
                    events(NetEvent::Audio { seq, data: packet.to_vec() });
                    continue;
                }
                last_fragment = Instant::now();
                let p = ra.push(&d);
                if p.dropped > 0 {
                    events(NetEvent::FramesDropped(p.dropped));
                    request_key(&mut last_key_req);
                }
                if let Some((meta, data)) = p.frame {
                    events(NetEvent::Frame { meta, data });
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(Ended::Quit)
        }
    };

    // Files, one at a time, each on its own stream beside the session.
    let (file_tx, mut file_rx) = mpsc::unbounded_channel::<(std::path::PathBuf, Option<(f32, f32)>)>();
    let files = {
        let conn = conn.clone();
        let events = events.clone();
        async move {
            while let Some((path, drop_at)) = file_rx.recv().await {
                let name = file_name(&path);
                let mut shown = Instant::now() - Duration::from_secs(1);
                let progress = |sent: u64, total: u64| {
                    if sent == total || shown.elapsed() >= Duration::from_millis(100) {
                        shown = Instant::now();
                        events(NetEvent::FileProgress { name: name.clone(), sent, total });
                    }
                };
                match crate::files::send(&conn, &path, drop_at, progress).await {
                    Ok(path) => events(NetEvent::FileSent { name, path }),
                    Err(e) => events(NetEvent::FileFailed { name, error: format!("{e:#}") }),
                }
            }
            // A failed file never ends the session.
            std::future::pending::<anyhow::Result<Ended>>().await
        }
    };

    // Viewer → host input, plus periodic pings.
    let input = {
        let conn = conn.clone();
        async move {
            let mut ping = tokio::time::interval(PING_INTERVAL);
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ping.tick() => write_msg(&mut send, &ClientMsg::Ping { t_client_us: crate::clock::now_us() }).await?,
                    m = rx.recv() => match m {
                        None | Some(Outgoing::Quit) => {
                            let _ = write_msg(&mut send, &ClientMsg::Bye).await;
                            send.finish();
                            let _ = tokio::time::timeout(Duration::from_millis(300), send.stopped()).await;
                            return anyhow::Ok(Ended::Quit);
                        }
                        Some(Outgoing::Msg(m)) => {
                            if let ClientMsg::UpdateSettings(s) = &m {
                                *settings = s.clone();
                            }
                            write_msg(&mut send, &m).await?;
                        }
                        Some(Outgoing::Motion(m)) => {
                            // Unreliable; a lost motion is superseded by the next one.
                            let _ = conn.send_datagram(m.encode().into());
                        }
                        Some(Outgoing::SendFile { path, drop_at }) => {
                            let _ = file_tx.send((path, drop_at));
                        }
                    }
                }
            }
        }
    };

    tokio::select! {
        r = control => r,
        r = video => r,
        r = input => r,
        r = files => r,
    }
}
