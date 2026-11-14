//! QUIC server: accepts paired clients, runs sessions, handles pairing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use ac_remote::config::{ClientConfig, HostConfig};
use ac_remote::identity::{Identity, Role};
use ac_remote::net::{self, Conn};
use ac_remote::pairing::{self, HostPairOutcome, PairingWindow};
use ac_remote::proto::{read_msg, write_msg, ClientMsg, ClipMsg, HostMsg, Motion};
use ac_remote::transport::{self, ALPN_PAIR, ALPN_PROBE, ALPN_SESSION};
use ac_remote::trust::{AuthorizedClient, AuthorizedClients, KnownHost, KnownHosts};
use ac_remote::video::{fragment, FrameMeta};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::input::Injector;
use crate::stream::{self, StreamControl, StreamEvent};

/// Release held input if the client is silent this long.
const INPUT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(1);

/// QUIC application close codes.
pub mod close {
    pub const NORMAL: u32 = 0;
    pub const NOT_PAIRED: u32 = 1;
    pub const REPLACED: u32 = 2;
    pub const VERSION: u32 = 3;
    pub const ERROR: u32 = 4;
    pub const SHUTDOWN: u32 = 5;
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PairEvent {
    Paired { name: String, fingerprint: String },
    WrongPin { attempts_left: u32 },
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatus {
    pub name: String,
    pub fingerprint: String,
    pub address: String,
    pub connected_secs: u64,
    pub monitor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub rtt_ms: f64,
}

struct Session {
    id: u64,
    conn: Conn,
    name: String,
    fingerprint: String,
    started: Instant,
    stream: Option<(String, u32, u32, u32)>,
    bytes_sent: Arc<AtomicU64>,
    last_rate: (Instant, u64, u32),
}

pub struct Host {
    pub config: HostConfig,
    pub identity: Identity,
    pub name: String,
    pub listen: SocketAddr,
    pairing: Mutex<Option<PairingWindow>>,
    /// Pairing over the internet: open while an invite code is shown.
    invite: Mutex<Option<PairingWindow>>,
    pub pair_events: broadcast::Sender<PairEvent>,
    session: Mutex<Option<Session>>,
    injector: Mutex<Option<Injector>>,
    /// The streamed monitor's place on the desktop, for files dropped on it.
    drop_target: Mutex<Option<crate::input::Target>>,
    next_id: AtomicU64,
    endpoint: Mutex<Option<quinn::Endpoint>>,
    internet: Mutex<Option<iroh::Endpoint>>,
}

/// The share service's state on the internet, for the PC tab.
#[derive(Debug, Clone, Serialize)]
pub struct InternetStatus {
    pub enabled: bool,
    pub id: Option<String>,
    /// Connected to a relay: other PCs can reach this one.
    pub online: bool,
    pub relay: Option<String>,
    pub error: Option<String>,
}

impl Host {
    pub fn new(config: HostConfig) -> Result<Arc<Self>> {
        let identity = Identity::load_or_create(Role::Host)?;
        let listen: SocketAddr = config.listen.parse().with_context(|| format!("invalid listen address {:?}", config.listen))?;
        Ok(Arc::new(Self {
            name: config.device_name(),
            config,
            identity,
            listen,
            pairing: Mutex::new(None),
            invite: Mutex::new(None),
            pair_events: broadcast::channel(16).0,
            session: Mutex::new(None),
            injector: Mutex::new(None),
            drop_target: Mutex::new(None),
            next_id: AtomicU64::new(1),
            endpoint: Mutex::new(None),
            internet: Mutex::new(None),
        }))
    }

    /// Create the virtual input devices up front: the compositor takes a moment
    /// to pick up new devices, which would otherwise eat the first keystrokes.
    pub fn init_input(&self) -> Result<()> {
        *self.injector.lock().unwrap() = Some(Injector::new()?);
        crate::local_input::start();
        Ok(())
    }

    /// Open a pairing window; returns the PIN.
    pub fn open_pairing(&self) -> PairingWindow {
        let w = PairingWindow::new();
        *self.pairing.lock().unwrap() = Some(w.clone());
        tracing::info!("pairing window open for {}s", w.remaining().as_secs());
        w
    }

    pub fn close_pairing(&self) {
        *self.pairing.lock().unwrap() = None;
    }

    /// Open a pairing window for the internet; returns the invite code.
    pub fn open_invite(&self) -> Result<(String, PairingWindow)> {
        let ep = self.internet.lock().unwrap().clone().context("access from other networks is off on this PC")?;
        let w = PairingWindow::with_lifetime(ac_remote::internet::INVITE_LIFETIME);
        let relay = ep.addr().relay_urls().next().cloned();
        let code = ac_remote::internet::Invite { id: ep.id(), pin: w.pin.clone(), relay }.encode();
        *self.invite.lock().unwrap() = Some(w.clone());
        tracing::info!("invite open for {}s", w.remaining().as_secs());
        Ok((code, w))
    }

    pub fn close_invite(&self) {
        *self.invite.lock().unwrap() = None;
    }

    fn invite_open(&self) -> bool {
        let mut p = self.invite.lock().unwrap();
        if p.as_ref().is_some_and(|w| w.expired()) {
            *p = None;
            let _ = self.pair_events.send(PairEvent::Expired);
        }
        p.is_some()
    }

    pub fn internet_status(&self) -> InternetStatus {
        let ep = self.internet.lock().unwrap().clone();
        let mut st = InternetStatus { enabled: self.config.internet, id: None, online: false, relay: None, error: None };
        if let Some(ep) = ep {
            use iroh::Watcher;
            st.id = Some(ep.id().to_string());
            let relays = ep.home_relay_status().get();
            st.online = relays.iter().any(|r| r.is_connected());
            st.relay = relays.first().map(|r| r.url().to_string());
            st.error = relays.iter().find_map(|r| r.last_error().map(|e| e.to_string())).filter(|_| !st.online);
        } else if self.config.internet {
            st.error = Some("starting".into());
        }
        st
    }

    pub fn pairing_open(&self) -> bool {
        let mut p = self.pairing.lock().unwrap();
        if p.as_ref().is_some_and(|w| w.expired()) {
            *p = None;
            let _ = self.pair_events.send(PairEvent::Expired);
        }
        p.is_some()
    }

    pub fn client_status(&self) -> Option<ClientStatus> {
        let mut s = self.session.lock().unwrap();
        let s = s.as_mut()?;
        let now = Instant::now();
        let bytes = s.bytes_sent.load(Ordering::Relaxed);
        let (t0, b0, kbps) = s.last_rate;
        let dt = now.duration_since(t0).as_secs_f64();
        let kbps = if dt >= 1.0 {
            let k = ((bytes - b0) as f64 * 8.0 / 1000.0 / dt) as u32;
            s.last_rate = (now, bytes, k);
            k
        } else {
            kbps
        };
        Some(ClientStatus {
            name: s.name.clone(),
            fingerprint: s.fingerprint.clone(),
            address: s.conn.describe(),
            connected_secs: s.started.elapsed().as_secs(),
            monitor: s.stream.as_ref().map(|x| x.0.clone()),
            width: s.stream.as_ref().map_or(0, |x| x.1),
            height: s.stream.as_ref().map_or(0, |x| x.2),
            fps: s.stream.as_ref().map_or(0, |x| x.3),
            kbps,
            rtt_ms: s.conn.rtt().as_secs_f64() * 1000.0,
        })
    }

    pub fn disconnect_client(&self) -> bool {
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            s.conn.close(close::NORMAL, b"disconnected by host");
            return true;
        }
        false
    }

    pub fn shutdown(&self) {
        if let Some(ep) = self.endpoint.lock().unwrap().as_ref() {
            ep.close(close::SHUTDOWN.into(), b"host shutting down");
        }
        if let Some(ep) = self.internet.lock().unwrap().take() {
            tokio::spawn(async move { ep.close().await });
        }
        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.release_all();
        }
    }

    /// Accept connections until the endpoint closes.
    pub async fn serve(self: &Arc<Self>) -> Result<()> {
        let ep = transport::server_endpoint(&self.identity, self.listen)?;
        tracing::info!("listening on {} as {:?} (fingerprint {})", ep.local_addr()?, self.name, ac_remote::identity::short_fingerprint(&self.identity.fingerprint));
        *self.endpoint.lock().unwrap() = Some(ep.clone());
        while let Some(incoming) = ep.accept().await {
            let host = self.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = host.handle(conn.into()).await {
                            tracing::warn!("{remote}: {e:#}");
                        }
                    }
                    Err(e) => tracing::debug!("{remote}: handshake failed: {e}"),
                }
            });
        }
        Ok(())
    }

    /// Accept PCs paired over the internet, if allowed (spec3). Runs next to
    /// [`Host::serve`] and never ends it: a failure here leaves the LAN alone.
    pub async fn serve_internet(self: &Arc<Self>) {
        if !self.config.internet {
            return;
        }
        let ep = match ac_remote::internet::host_endpoint(&self.config.relay).await {
            Ok(ep) => ep,
            Err(e) => return tracing::error!("access from other networks disabled: {e:#}"),
        };
        tracing::info!("reachable from other networks as {}", net::short_id(&ep.id()));
        *self.internet.lock().unwrap() = Some(ep.clone());
        while let Some(incoming) = ep.accept().await {
            let host = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        let conn = Conn::from(conn);
                        let who = conn.describe();
                        if let Err(e) = host.handle(conn).await {
                            tracing::warn!("{who}: {e:#}");
                        }
                    }
                    Err(e) => tracing::debug!("internet handshake failed: {e}"),
                }
            });
        }
    }

    async fn handle(self: &Arc<Self>, conn: Conn) -> Result<()> {
        let internet = matches!(conn, Conn::Internet(_));
        let fp = conn.peer_key()?;
        match conn.alpn().as_deref() {
            Some(ALPN_PAIR) => self.handle_pair(conn).await,
            Some(ALPN_PROBE) => {
                conn.close(close::NORMAL, b"ok");
                Ok(())
            }
            Some(ALPN_SESSION) => {
                let clients = if internet { AuthorizedClients::load_internet()? } else { AuthorizedClients::load()? };
                let Some(client) = clients.find_fingerprint(&fp).cloned() else {
                    tracing::warn!("rejecting unpaired client {} ({})", conn.describe(), ac_remote::identity::short_fingerprint(&fp));
                    conn.close(close::NOT_PAIRED, b"not paired with this host");
                    return Ok(());
                };
                self.handle_session(conn, client).await
            }
            _ => {
                conn.close(close::VERSION, b"unknown protocol");
                Ok(())
            }
        }
    }

    async fn handle_pair(self: &Arc<Self>, conn: Conn) -> Result<()> {
        // A LAN code pairs LAN connections only, an invite code internet ones.
        let internet_id = match &conn {
            Conn::Internet(_) => Some(self.internet.lock().unwrap().as_ref().map(|ep| ep.id().to_string()).context("internet endpoint closed")?),
            Conn::Lan(_) => None,
        };
        let window = if internet_id.is_some() { &self.invite } else { &self.pairing };
        let open = if internet_id.is_some() { self.invite_open() } else { self.pairing_open() };
        let pin = if open { window.lock().unwrap().as_ref().map(|w| w.pin.clone()) } else { None };
        let own = internet_id.clone().unwrap_or_else(|| self.identity.fingerprint.clone());
        let outcome = pairing::pair_as_host(&conn, &own, &self.name, pin.as_deref()).await?;
        match outcome {
            HostPairOutcome::Paired(p) => {
                let mut store = if internet_id.is_some() { AuthorizedClients::load_internet()? } else { AuthorizedClients::load()? };
                store.upsert(AuthorizedClient { name: p.peer_name.clone(), fingerprint: p.peer_fingerprint.clone(), paired_at: ac_remote::unix_now() });
                store.save()?;
                *window.lock().unwrap() = None;
                tracing::info!("paired with {:?}", p.peer_name);
                let _ = self.pair_events.send(PairEvent::Paired { name: p.peer_name, fingerprint: p.peer_fingerprint });
            }
            HostPairOutcome::WrongPin => {
                let mut g = window.lock().unwrap();
                if let Some(w) = g.as_mut() {
                    w.attempts += 1;
                    let left = pairing::MAX_ATTEMPTS.saturating_sub(w.attempts);
                    tracing::warn!("wrong pairing PIN from {} ({left} attempts left)", conn.describe());
                    let _ = self.pair_events.send(PairEvent::WrongPin { attempts_left: left });
                    if left == 0 {
                        *g = None;
                        let _ = self.pair_events.send(PairEvent::Expired);
                    }
                }
            }
            HostPairOutcome::Refused(r) => tracing::info!("pairing refused: {r}"),
        }
        conn.close(close::NORMAL, b"");
        Ok(())
    }

    async fn handle_session(self: &Arc<Self>, conn: Conn, client: AuthorizedClient) -> Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let (settings, client_name) = match read_msg::<ClientMsg>(&mut recv).await? {
            Some(ClientMsg::Hello { version, client_name, settings }) => {
                if version != ac_remote::PROTOCOL_VERSION {
                    let text = format!("protocol version mismatch: host {} client {version}", ac_remote::PROTOCOL_VERSION);
                    write_msg(&mut send, &HostMsg::Error { text }).await?;
                    conn.close(close::VERSION, b"version mismatch");
                    return Ok(());
                }
                (settings, client_name)
            }
            other => anyhow::bail!("expected Hello, got {other:?}"),
        };
        let monitors: Vec<_> = crate::monitors::list().map(|m| m.into_iter().map(|m| m.info).collect()).unwrap_or_default();
        write_msg(&mut send, &HostMsg::Welcome { version: ac_remote::PROTOCOL_VERSION, host_name: self.name.clone(), monitors }).await?;

        // One client at a time: a new connection from a paired client replaces
        // the old one (typically the same laptop reconnecting after a drop).
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes_sent = Arc::new(AtomicU64::new(0));
        {
            let mut s = self.session.lock().unwrap();
            if let Some(old) = s.take() {
                tracing::info!("replacing session with {:?}", old.name);
                old.conn.close(close::REPLACED, b"replaced by a new connection");
            }
            *s = Some(Session {
                id,
                conn: conn.clone(),
                name: client.name.clone(),
                fingerprint: client.fingerprint.clone(),
                started: Instant::now(),
                stream: None,
                bytes_sent: bytes_sent.clone(),
                last_rate: (Instant::now(), 0, 0),
            });
        }
        tracing::info!("session started: {:?} ({}) from {}", client.name, client_name, conn.describe());
        if self.config.notify_on_connect {
            notify(&format!("{} connected", client.name));
        }
        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.new_session();
        }

        let result = self.run_session(&conn, send, recv, settings, id, bytes_sent).await;

        if let Some(i) = self.injector.lock().unwrap().as_mut() {
            i.release_all();
        }
        {
            let mut s = self.session.lock().unwrap();
            if s.as_ref().is_some_and(|s| s.id == id) {
                *s = None;
            }
        }
        tracing::info!("session ended: {:?} ({})", client.name, match &result { Ok(()) => "closed".to_string(), Err(e) => format!("{e:#}") });
        if self.config.notify_on_connect {
            notify(&format!("{} disconnected", client.name));
        }
        result
    }

    async fn run_session(
        self: &Arc<Self>,
        conn: &Conn,
        send: net::SendStream,
        mut recv: net::RecvStream,
        settings: ac_remote::proto::StreamSettings,
        id: u64,
        bytes_sent: Arc<AtomicU64>,
    ) -> Result<()> {
        let ctrl = Arc::new(StreamControl::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        stream::spawn(self.config.clone(), settings, ctrl.clone(), tx);
        struct StopOnDrop(Arc<StreamControl>);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.stop.store(true, Ordering::Relaxed);
            }
        }
        let _stop = StopOnDrop(ctrl.clone());

        // Single writer for the control stream.
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostMsg>();
        let writer = async move {
            let mut send = send;
            while let Some(m) = out_rx.recv().await {
                write_msg(&mut send, &m).await?;
            }
            anyhow::Ok(())
        };

        let audio_bytes = bytes_sent.clone();
        let video = {
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let host = self.clone();
            async move {
                let mut frame_id: u32 = 0;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        StreamEvent::Info { monitor, width, height, fps, target } => {
                            if let Some(i) = host.injector.lock().unwrap().as_mut() {
                                i.set_target(target);
                            }
                            *host.drop_target.lock().unwrap() = Some(target);
                            if let Some(s) = host.session.lock().unwrap().as_mut().filter(|s| s.id == id) {
                                s.stream = Some((monitor.clone(), width, height, fps));
                            }
                            let _ = out_tx.send(HostMsg::StreamInfo { monitor, width, height, fps });
                        }
                        StreamEvent::Frame { data, keyframe, capture_us, host_us } => {
                            let max = conn.max_datagram_size().unwrap_or(1200);
                            let meta = FrameMeta { frame_id, keyframe, capture_us, host_us };
                            frame_id = frame_id.wrapping_add(1);
                            for d in fragment(meta, &data, max) {
                                bytes_sent.fetch_add(d.len() as u64, Ordering::Relaxed);
                                if let Err(e) = conn.send_datagram(Bytes::from(d)) {
                                    anyhow::bail!("sending video: {e}");
                                }
                            }
                        }
                        StreamEvent::Error(text) => {
                            let _ = out_tx.send(HostMsg::Error { text: text.clone() });
                            anyhow::bail!("stream failed: {text}");
                        }
                    }
                }
                anyhow::Ok(())
            }
        };

        // Desktop audio. Optional: a failure is reported, never fatal.
        let audio = {
            let conn = conn.clone();
            let out_tx = out_tx.clone();
            let enabled = self.config.audio;
            let bytes_sent = audio_bytes;
            async move {
                if !enabled {
                    return std::future::pending().await;
                }
                let (tx, mut rx) = mpsc::unbounded_channel();
                let (err_tx, mut err_rx) = mpsc::unbounded_channel();
                crate::audio::spawn(tx, err_tx);
                let mut seq: u32 = 0;
                loop {
                    tokio::select! {
                        Some(p) = rx.recv() => {
                            let d = ac_remote::proto::encode_audio(seq, &p);
                            seq = seq.wrapping_add(1);
                            bytes_sent.fetch_add(d.len() as u64, Ordering::Relaxed);
                            if let Err(e) = conn.send_datagram(Bytes::from(d)) {
                                anyhow::bail!("sending audio: {e}");
                            }
                        }
                        Some(e) = err_rx.recv() => {
                            let _ = out_tx.send(HostMsg::Notice { text: format!("No audio: {e}") });
                        }
                        else => return std::future::pending().await,
                    }
                }
            }
        };

        let motion = {
            let conn = conn.clone();
            let host = self.clone();
            async move {
                loop {
                    let d = conn.read_datagram().await?;
                    if let Some(m) = Motion::decode(&d).filter(|_| !crate::local_input::active()) {
                        if let Some(i) = host.injector.lock().unwrap().as_mut() {
                            let r = match m {
                                Motion::Abs { seq, x, y } => i.motion_abs(seq, x, y),
                                Motion::Rel { seq, dx, dy } => i.motion_rel(seq, dx, dy),
                            };
                            if let Err(e) = r {
                                tracing::warn!("pointer injection: {e}");
                            }
                        }
                    }
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            }
        };

        let local_tx = out_tx.clone();
        let clip_tx = out_tx.clone();
        let control = {
            let host = self.clone();
            let ctrl = ctrl.clone();
            let peer = conn.remote_address();
            let mut switch_to: Option<String> = None;
            async move {
                loop {
                    // Watchdog: the client pings every 250 ms. If it goes
                    // quiet, release held keys long before QUIC gives up.
                    let msg = match tokio::time::timeout(INPUT_WATCHDOG, read_msg::<ClientMsg>(&mut recv)).await {
                        Ok(r) => match r? {
                            Some(m) => m,
                            None => break,
                        },
                        Err(_) => {
                            if let Some(i) = host.injector.lock().unwrap().as_mut() {
                                i.release_all();
                            }
                            continue;
                        }
                    };
                    // Someone at this PC comes first: drop the remote's input.
                    let inject = |f: &mut dyn FnMut(&mut Injector) -> Result<()>| {
                        if crate::local_input::active() {
                            return;
                        }
                        if let Some(i) = host.injector.lock().unwrap().as_mut() {
                            if let Err(e) = f(i) {
                                tracing::warn!("input injection: {e}");
                            }
                        }
                    };
                    match msg {
                        ClientMsg::Key { code, pressed } => inject(&mut |i| i.key(code, pressed)),
                        ClientMsg::Button { code, pressed } => inject(&mut |i| i.button(code, pressed)),
                        ClientMsg::Scroll { v120_x, v120_y } => inject(&mut |i| i.scroll(v120_x, v120_y)),
                        ClientMsg::ReleaseAll => {
                            if let Some(i) = host.injector.lock().unwrap().as_mut() {
                                i.release_all();
                            }
                        }
                        ClientMsg::RequestKeyframe => ctrl.keyframe.store(true, Ordering::Relaxed),
                        ClientMsg::UpdateSettings(s) => *ctrl.settings.lock().unwrap() = Some(s),
                        ClientMsg::Ping { t_client_us } => {
                            let _ = out_tx.send(HostMsg::Pong { t_client_us, t_host_us: ac_remote::clock::now_us() });
                        }
                        ClientMsg::Bye => break,
                        ClientMsg::Hello { .. } => tracing::warn!("duplicate Hello ignored"),
                        ClientMsg::SwitchSides { host_name, host_fingerprint, port } => {
                            let reply = match peer
                                .ok_or_else(|| anyhow::anyhow!("switching sides works on the local network only"))
                                .and_then(|peer| accept_switch(&host_name, &host_fingerprint, SocketAddr::new(peer.ip().to_canonical(), port)))
                            {
                                Ok((client_name, client_fingerprint)) => {
                                    switch_to = Some(host_fingerprint);
                                    HostMsg::SwitchReady { client_name, client_fingerprint }
                                }
                                Err(e) => {
                                    tracing::warn!("can't switch sides with {host_name:?}: {e:#}");
                                    HostMsg::SwitchRefused { text: format!("{e:#}") }
                                }
                            };
                            let _ = out_tx.send(reply);
                        }
                        ClientMsg::SwitchNow => match switch_to.take() {
                            Some(fp) => {
                                if let Err(e) = open_viewer(&fp) {
                                    tracing::warn!("opening Test Bed to switch sides: {e:#}");
                                    let _ = out_tx.send(HostMsg::SwitchRefused { text: format!("{e:#}") });
                                }
                            }
                            None => tracing::warn!("SwitchNow without SwitchSides ignored"),
                        },
                    }
                }
                anyhow::Ok(())
            }
        };

        // Files from the client, each on a stream of its own.
        let files = {
            let conn = conn.clone();
            let host = self.clone();
            async move {
                loop {
                    let (send, recv) = conn.accept_bi().await?;
                    let target = *host.drop_target.lock().unwrap();
                    tokio::spawn(async move {
                        match ac_remote::files::receive(send, recv, move |at| crate::drop_folder::folder_for(at, target)).await {
                            Ok(path) => {
                                tracing::info!("received {}", path.display());
                                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                                notify(&format!("Received {name} in {folder}"));
                            }
                            Err(e) => tracing::warn!("receiving a file: {e:#}"),
                        }
                    });
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            }
        };

        // When someone starts using this PC, let go of what the remote holds
        // and tell it; tell it again once they stop.
        let local = {
            let host = self.clone();
            let out_tx = local_tx;
            async move {
                let mut paused = false;
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
                loop {
                    tick.tick().await;
                    let active = crate::local_input::active();
                    if active == paused {
                        continue;
                    }
                    paused = active;
                    let text = if active {
                        if let Some(i) = host.injector.lock().unwrap().as_mut() {
                            i.release_all();
                        }
                        format!("Someone is using {}: your keyboard and mouse are paused until they stop", host.name)
                    } else {
                        format!("You have {} back", host.name)
                    };
                    tracing::info!("{text}");
                    let _ = out_tx.send(HostMsg::Notice { text });
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            }
        };

        // The clipboard, for a viewer that subscribes (the phone app).
        let clipboard = {
            let conn = conn.clone();
            let out_tx = clip_tx;
            async move {
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                let mut watch = None;
                // The last text either way, so nothing echoes back.
                let mut last: Option<String> = None;
                loop {
                    tokio::select! {
                        r = conn.accept_uni() => match ac_remote::proto::read_clip(r?).await {
                            Ok(Some(ClipMsg::Subscribe)) if watch.is_none() => match crate::clipboard::watch(tx.clone()) {
                                Ok(w) => {
                                    tracing::info!("sharing the clipboard with the viewer");
                                    watch = Some(w);
                                }
                                Err(e) => {
                                    tracing::warn!("watching the clipboard: {e:#}");
                                    let _ = out_tx.send(HostMsg::Notice { text: format!("The clipboard isn't shared: {e:#}") });
                                }
                            },
                            Ok(Some(ClipMsg::Text { text })) => {
                                tracing::info!("clipboard from the viewer ({} characters)", text.chars().count());
                                last = Some(text.clone());
                                let r = tokio::task::spawn_blocking(move || crate::clipboard::set(&text)).await;
                                if let Err(e) = r.map_err(anyhow::Error::from).and_then(|r| r) {
                                    tracing::warn!("setting the clipboard: {e:#}");
                                }
                            }
                            Ok(_) => {}
                            Err(e) => tracing::debug!("clipboard from the viewer: {e:#}"),
                        },
                        Some(text) = rx.recv() => {
                            if last.as_deref() != Some(text.as_str()) {
                                last = Some(text.clone());
                                tracing::info!("clipboard to the viewer ({} characters)", text.chars().count());
                                let msg = ClipMsg::Text { text: ac_remote::proto::clip_text(&text) };
                                if let Err(e) = ac_remote::proto::send_clip(&conn, &msg).await {
                                    tracing::debug!("clipboard to the viewer: {e:#}");
                                }
                            }
                        }
                    }
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            }
        };

        let res = tokio::select! {
            r = clipboard => r,
            r = local => r,
            r = files => r,
            r = writer => r,
            r = video => r,
            r = audio => r,
            r = motion => r,
            r = control => r,
        };
        match res {
            Err(e) if net::is_closed(&e) => Ok(()),
            Err(e) => {
                let mut reason = format!("{e:#}");
                reason.truncate(500);
                conn.close(close::ERROR, reason.as_bytes());
                Err(e)
            }
            Ok(()) => {
                conn.close(close::NORMAL, b"session ended");
                Ok(())
            }
        }
    }
}

/// Switching sides, step one: trust the client's own shared PC (the client
/// already controls this one, so its word is good), and say which identity
/// this PC's viewer will connect back with.
fn accept_switch(name: &str, fingerprint: &str, address: SocketAddr) -> Result<(String, String)> {
    anyhow::ensure!(viewer_binary().is_some(), "Test Bed isn't installed on this PC");
    let mut known = KnownHosts::load()?;
    known.upsert(KnownHost { name: name.into(), address: address.to_string(), fingerprint: fingerprint.into(), paired_at: ac_remote::unix_now(), last_seen: None, internet: false });
    known.save()?;
    let me = Identity::load_or_create(Role::Client)?;
    let my_name = ClientConfig::load().map(|c| c.device_name()).unwrap_or_else(|_| ac_remote::hostname());
    Ok((my_name, me.fingerprint))
}

/// Test Bed, installed next to this service.
fn viewer_binary() -> Option<std::path::PathBuf> {
    const BINARY: &str = if cfg!(windows) { "test-bed.exe" } else { "test-bed" };
    if let Some(p) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join(BINARY))).filter(|p| p.exists()) {
        return Some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(BINARY)).find(|p| p.exists())
}

/// Switching sides, step two: Test Bed on this PC connects to the client.
/// It runs in its own scope, so restarting this service leaves it open.
fn open_viewer(fingerprint: &str) -> Result<()> {
    let bin = viewer_binary().context("Test Bed isn't installed on this PC")?;
    let quiet = |c: &mut std::process::Command| {
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    };
    let mut c = std::process::Command::new("systemd-run");
    c.args(["--user", "--scope", "--collect", "--quiet", "--"]).arg(&bin).args(["--connect", fingerprint]);
    quiet(&mut c);
    let child = match c.spawn() {
        Ok(child) => child,
        Err(_) => {
            let mut c = std::process::Command::new(&bin);
            c.args(["--connect", fingerprint]);
            quiet(&mut c);
            c.spawn().with_context(|| format!("starting {}", bin.display()))?
        }
    };
    // Reap it: when Test Bed is already open, this one hands over and exits.
    let mut child = child;
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Test Bed", "-i", "network-wired", "Test Bed", text])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
