//! A remote PC shown in the PC tab: the picture, its toolbar (one button per
//! remote monitor, Fullscreen) and keyboard and mouse forwarding. The
//! connection is RemoteLinux's (`ac_remote::connection`); frames are decoded
//! like a phone's (VA-API when available) and drawn with graphics offload.
//!
//! Chords (Ctrl+Alt+Shift with): Q releases or takes back keyboard and mouse,
//! F fullscreen, D disconnect, S switch sides, 1–9 switch monitor. They never
//! reach the PC.
//!
//! Switch sides hands control the other way: this PC is shared (sharing is
//! turned on if it was off), the two PCs trust each other's sharing over this
//! already paired connection, and the other PC's share service opens Test
//! Bed there, connected back to this PC. Then this viewer closes.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ac_input::Letterbox;
use ac_input::pc_keys::evdev_from_gtk;
use ac_media::{Backend, Decoder, HwOutput, Mailbox, MediaContext};
use ac_remote::connection::{self, NetEvent, NetHandle, Outgoing};
use ac_remote::proto::{ClientMsg, MonitorInfo, Motion, StreamSettings};
use ac_remote::hostctl;
use ac_remote::trust::{AuthorizedClient, AuthorizedClients, KnownHost};
use ac_remote::video::FrameMeta;
use ac_scrcpy::VideoCodec;
use adw::prelude::*;
use gtk::{gdk, glib};

use crate::texture;

/// Decode errors within this window that make hardware decoding give up.
const ERROR_WINDOW: Duration = Duration::from_secs(2);
const ERROR_BURST: usize = 8;
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// Touchpad scrolling: pixels per wheel notch.
const PIXELS_PER_NOTCH: f64 = 20.0;
/// A PC that hasn't answered a switch by then can't switch (RemoteLinux).
const SWITCH_TIMEOUT: Duration = Duration::from_secs(5);
const CHORD: gdk::ModifierType =
    gdk::ModifierType::CONTROL_MASK.union(gdk::ModifierType::ALT_MASK).union(gdk::ModifierType::SHIFT_MASK);

/// What the viewer asks of the PC tab.
pub trait ViewerHost {
    fn viewer_closed(&self);
    fn viewer_fullscreen(&self, on: bool);
    /// Keyboard and mouse go to the remote PC: Test Bed's own shortcuts
    /// must let keys through.
    fn viewer_grabbing(&self, on: bool);
    /// A message for the PC tab's toasts.
    fn viewer_message(&self, text: &str);
}

#[derive(Default)]
struct State {
    connected: bool,
    fatal: bool,
    forwarding: bool,
    monitors: Vec<MonitorInfo>,
    monitor: String,
    stream: (u32, u32),
    motion_seq: u32,
    /// evdev codes held down on the PC.
    keys: HashSet<u16>,
    /// evdev mouse buttons held down on the PC.
    buttons: HashSet<u16>,
    /// GTK keycodes whose release belongs to a chord.
    swallowed: HashSet<u32>,
    /// Mouse buttons whose release must not reach the PC.
    swallowed_buttons: HashSet<u32>,
    wheel: (f64, f64),
    /// Host clock minus ours, for the latency figure.
    offset_us: Option<i64>,
    rtt_us: u64,
    frames: u32,
    latency_us: u64,
    latency_n: u32,
    stats_at: Option<Instant>,
    seq: u64,
    grabbing: bool,
    fullscreen: bool,
    /// Switching sides, since.
    switching: Option<Instant>,
    /// Files handed to the connection and not yet saved or failed.
    files_pending: u32,
}

pub struct Viewer {
    pub root: gtk::Box,
    host: KnownHost,
    net: NetHandle,
    mailbox: Arc<Mailbox>,
    media: Option<Arc<MediaContext>>,
    window: adw::ApplicationWindow,
    picture: gtk::Picture,
    overlay_status: gtk::Label,
    overlay_detail: gtk::Label,
    overlay_box: gtk::Box,
    hint: gtk::Label,
    monitors_box: gtk::Box,
    stats: gtk::Label,
    fullscreen_button: gtk::Button,
    switch_button: gtk::Button,
    send_button: gtk::Button,
    transfer: gtk::Label,
    state: RefCell<State>,
    keys: RefCell<Option<gtk::EventControllerKey>>,
    window_active: RefCell<Option<glib::SignalHandlerId>>,
    tab: Rc<dyn ViewerHost>,
    closed: Cell<bool>,
}

impl Viewer {
    pub fn open(
        host: KnownHost,
        client_name: String,
        window: &adw::ApplicationWindow,
        media: Option<Arc<MediaContext>>,
        offload: bool,
        tab: Rc<dyn ViewerHost>,
    ) -> anyhow::Result<Rc<Self>> {
        let mailbox = Arc::new(Mailbox::default());
        let (ui_tx, ui_rx) = async_channel::unbounded::<NetEvent>();
        let (frame_tx, frame_rx) = mpsc::channel::<(FrameMeta, Vec<u8>)>();
        // Muted until asked: the sound stays on the PC being controlled.
        let muted = Arc::new(AtomicBool::new(true));
        let audio_muted = muted.clone();
        let audio = ac_remote::playback::spawn(&host.name).map_err(|e| tracing::warn!("remote audio: {e:#}")).ok();
        let net = connection::spawn(host.clone(), StreamSettings::default(), client_name, move |ev| match ev {
            NetEvent::Frame { meta, data } => {
                let _ = frame_tx.send((meta, data));
            }
            NetEvent::Audio { seq, data } => {
                if let Some(a) = audio.as_ref().filter(|_| !audio_muted.load(Ordering::Relaxed)) {
                    // Full: the player is behind, and this packet would only add delay.
                    let _ = a.try_send((seq, data));
                }
            }
            other => {
                let _ = ui_tx.try_send(other);
            }
        })?;
        {
            let (mailbox, media, tx) = (mailbox.clone(), media.clone(), net.tx.clone());
            std::thread::Builder::new().name("remote-decode".into()).spawn(move || decode_loop(frame_rx, &mailbox, media.as_deref(), &tx))?;
        }

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .build();
        let offloaded = gtk::GraphicsOffload::new(Some(&picture));
        offloaded.set_black_background(true);
        offloaded.set_enabled(if offload { gtk::GraphicsOffloadEnabled::Enabled } else { gtk::GraphicsOffloadEnabled::Disabled });
        offloaded.add_css_class("remote-video");

        let overlay_status = gtk::Label::builder().css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build();
        let overlay_detail = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).max_width_chars(60).build();
        overlay_detail.add_css_class("dim-label");
        let back = gtk::Button::builder().label("Back to computers").halign(gtk::Align::Center).visible(false).build();
        back.add_css_class("pill");
        let overlay_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["remote-panel"])
            .build();
        overlay_box.append(&overlay_status);
        overlay_box.append(&overlay_detail);
        overlay_box.append(&back);
        let hint = gtk::Label::builder()
            .label("Click to control this PC · Ctrl+Alt+Shift+Q releases it")
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .visible(false)
            .css_classes(["remote-panel"])
            .can_target(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&offloaded).hexpand(true).vexpand(true).build();
        overlay.add_overlay(&overlay_box);
        overlay.add_overlay(&hint);

        let disconnect = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Disconnect (Ctrl+Alt+Shift+D)").build();
        let title = gtk::Label::builder().label(&host.name).css_classes(["heading"]).build();
        let monitors_box = gtk::Box::builder().spacing(4).margin_start(12).build();
        let stats = gtk::Label::builder().hexpand(true).halign(gtk::Align::End).css_classes(["dim-label", "numeric"]).build();
        let switch_button = gtk::Button::builder()
            .label("Switch sides")
            .tooltip_text(format!("Let {} control this PC instead (Ctrl+Alt+Shift+S)", host.name))
            .sensitive(false)
            // Switching sides works on the local network only (spec3 §6).
            .visible(!host.internet)
            .build();
        let send_button = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("document-send-symbolic").label("Send file").build())
            .tooltip_text(format!("Send files to the Downloads folder on {}. You can also drop files on its screen.", host.name))
            .sensitive(false)
            .build();
        let transfer = gtk::Label::builder().visible(false).css_classes(["dim-label", "numeric"]).build();
        let mute_button = gtk::ToggleButton::builder()
            .icon_name("audio-volume-muted-symbolic")
            .tooltip_text("Sound muted: click to hear this PC")
            .active(true)
            .build();
        mute_button.connect_toggled(move |b| {
            let on = b.is_active();
            muted.store(on, Ordering::Relaxed);
            b.set_icon_name(if on { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
            b.set_tooltip_text(Some(if on { "Sound muted: click to hear this PC" } else { "Mute this PC's sound" }));
        });
        let fullscreen_button = gtk::Button::builder().icon_name("view-fullscreen-symbolic").tooltip_text("Fullscreen (Ctrl+Alt+Shift+F)").build();
        let bar = gtk::Box::builder().spacing(6).css_classes(["remote-bar"]).build();
        bar.append(&disconnect);
        bar.append(&title);
        bar.append(&monitors_box);
        bar.append(&stats);
        bar.append(&transfer);
        bar.append(&send_button);
        bar.append(&switch_button);
        bar.append(&mute_button);
        bar.append(&fullscreen_button);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&bar);
        root.append(&overlay);

        let v = Rc::new(Self {
            root,
            host,
            net,
            mailbox,
            media,
            window: window.clone(),
            picture,
            overlay_status,
            overlay_detail,
            overlay_box,
            hint,
            monitors_box,
            stats,
            fullscreen_button,
            switch_button,
            send_button,
            transfer,
            state: RefCell::new(State { forwarding: true, ..Default::default() }),
            keys: RefCell::default(),
            window_active: RefCell::default(),
            tab,
            closed: Cell::new(false),
        });
        v.show_status(&format!("Connecting to {}…", v.host.name), "");

        for b in [&disconnect, &back] {
            let weak = Rc::downgrade(&v);
            b.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.close();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            v.switch_button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.switch_sides();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            v.send_button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.choose_files();
                }
            });
        }
        v.attach_drop();
        {
            let weak = Rc::downgrade(&v);
            v.fullscreen_button.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.toggle_fullscreen();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            glib::spawn_future_local(async move {
                while let Ok(ev) = ui_rx.recv().await {
                    let Some(v) = weak.upgrade() else { break };
                    if let NetEvent::Fatal(_) = &ev {
                        back.set_visible(true);
                    }
                    v.on_net(ev);
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            v.picture.add_tick_callback(move |_, _| match weak.upgrade() {
                Some(v) => {
                    v.present_frame();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        v.attach_pointer();
        v.attach_keys();
        Ok(v)
    }

    pub fn host_name(&self) -> &str {
        &self.host.name
    }

    pub fn fingerprint(&self) -> &str {
        &self.host.fingerprint
    }

    fn send(&self, m: ClientMsg) {
        let _ = self.net.tx.send(Outgoing::Msg(m));
    }

    /// Keyboard and mouse go to the PC.
    fn active(&self) -> bool {
        let s = self.state.borrow();
        s.connected && s.forwarding && self.window.is_active() && !self.closed.get()
    }

    /// Grabs or lets go of the keyboard to match [`Self::active`]: Hyprland's
    /// own shortcuts are inhibited while the PC has the keyboard.
    fn update_grab(&self) {
        let on = self.active();
        let was = std::mem::replace(&mut self.state.borrow_mut().grabbing, on);
        if on != was {
            if let Some(top) = self.window.surface().and_downcast::<gdk::Toplevel>() {
                if on {
                    top.inhibit_system_shortcuts(None::<&gdk::Event>);
                } else {
                    top.restore_system_shortcuts();
                }
            }
            self.tab.viewer_grabbing(on);
        }
        let s = self.state.borrow();
        self.hint.set_visible(s.connected && !s.forwarding);
    }

    fn release_all(&self) {
        let had = {
            let mut s = self.state.borrow_mut();
            let had = !s.keys.is_empty() || s.connected;
            s.keys.clear();
            s.buttons.clear();
            had
        };
        if had {
            self.send(ClientMsg::ReleaseAll);
        }
    }

    fn set_forwarding(&self, on: bool) {
        if !on && self.active() {
            self.release_all();
        }
        self.state.borrow_mut().forwarding = on;
        self.update_grab();
    }

    fn show_status(&self, title: &str, detail: &str) {
        self.overlay_status.set_label(title);
        self.overlay_detail.set_label(detail);
        self.overlay_detail.set_visible(!detail.is_empty());
        self.overlay_box.set_visible(!title.is_empty());
    }

    fn on_net(self: &Rc<Self>, ev: NetEvent) {
        match ev {
            NetEvent::Connecting { .. } => {
                if !self.state.borrow().connected {
                    self.show_status(&format!("Connecting to {}…", self.host.name), "");
                }
            }
            NetEvent::Connected { mut monitors, .. } => {
                monitors.sort_by_key(|m| (m.y, m.x));
                {
                    let mut s = self.state.borrow_mut();
                    s.connected = true;
                    s.monitors = monitors;
                }
                self.show_status("", "");
                self.switch_button.set_sensitive(true);
                self.send_button.set_sensitive(true);
                self.rebuild_monitors();
                self.update_grab();
            }
            NetEvent::Stream { monitor, width, height, .. } => {
                {
                    let mut s = self.state.borrow_mut();
                    s.monitor = monitor;
                    s.stream = (width, height);
                }
                self.mailbox.set_size(width, height);
                self.rebuild_monitors();
            }
            NetEvent::Clock { rtt_us, offset_us } => {
                let mut s = self.state.borrow_mut();
                s.rtt_us = rtt_us;
                if offset_us != i64::MIN {
                    s.offset_us = Some(offset_us);
                }
            }
            NetEvent::FramesDropped(_) => {}
            NetEvent::Notice(text) => self.flash(&text),
            NetEvent::Disconnected { reason } => {
                self.switch_failed(&format!("{} needs Test Bed to switch sides", self.host.name));
                self.state.borrow_mut().connected = false;
                self.update_grab();
                self.show_status("Disconnected", &reason);
            }
            NetEvent::Reconnecting { retry_in, reason, .. } => {
                self.switch_failed(&format!("{} needs Test Bed to switch sides", self.host.name));
                self.state.borrow_mut().connected = false;
                self.update_grab();
                self.show_status(&format!("Reconnecting in {:.0} s…", retry_in.as_secs_f32().ceil()), &reason);
            }
            NetEvent::Fatal(msg) => {
                self.switch_failed(&msg);
                {
                    let mut s = self.state.borrow_mut();
                    s.connected = false;
                    s.fatal = true;
                }
                self.update_grab();
                tracing::warn!("remote {}: {msg}", self.host.name);
                self.show_status("Can't connect", &msg);
            }
            NetEvent::SwitchReady { client_name, client_fingerprint } => self.switch_ready(client_name, client_fingerprint),
            NetEvent::FileProgress { name, sent, total } => {
                let pct = (sent * 100).checked_div(total).unwrap_or(100);
                let waiting = self.state.borrow().files_pending.saturating_sub(1);
                let more = if waiting > 0 { format!(" (+{waiting})") } else { String::new() };
                self.transfer.set_label(&format!("Sending {name} · {pct}%{more}"));
                self.transfer.set_visible(true);
            }
            NetEvent::FileSent { name, path } => {
                self.file_done();
                self.tab.viewer_message(&format!("Sent {name} to {}: {path}", self.host.name));
            }
            NetEvent::FileFailed { name, error } => {
                self.file_done();
                self.tab.viewer_message(&format!("Couldn't send {name}: {error}"));
            }
            NetEvent::SwitchRefused(text) => {
                if self.state.borrow().switching.is_some() {
                    self.switch_failed(&text);
                } else {
                    self.tab.viewer_message(&format!("{} couldn't open Test Bed: {text}", self.host.name));
                }
            }
            NetEvent::Frame { .. } | NetEvent::Audio { .. } => {}
        }
        if !self.state.borrow().connected {
            self.switch_button.set_sensitive(false);
            self.send_button.set_sensitive(false);
        }
    }

    /// Lets the PC on the other end control this one instead.
    pub fn switch_sides(self: &Rc<Self>) {
        {
            let mut s = self.state.borrow_mut();
            if !s.connected || s.switching.is_some() {
                return;
            }
            s.switching = Some(Instant::now());
        }
        let started = self.state.borrow().switching;
        self.switch_button.set_sensitive(false);
        self.show_status("Switching sides…", "");
        let weak = Rc::downgrade(self);
        crate::remote::share_now(move |r| {
            let Some(v) = weak.upgrade() else { return };
            if v.state.borrow().switching != started {
                return;
            }
            let me = match r {
                Ok(me) => me,
                Err(e) => return v.switch_failed(&format!("couldn't share this PC: {e:#}")),
            };
            v.send(ClientMsg::SwitchSides { host_name: me.name, host_fingerprint: me.fingerprint, port: me.port });
            let weak = Rc::downgrade(&v);
            glib::timeout_add_local_once(SWITCH_TIMEOUT, move || {
                if let Some(v) = weak.upgrade()
                    && v.state.borrow().switching == started
                {
                    v.switch_failed(&format!("{} didn't answer; it needs Test Bed to switch sides", v.host.name));
                }
            });
        });
    }

    /// The other PC agreed: let its viewer in, tell it to connect, and close.
    fn switch_ready(&self, client_name: String, client_fingerprint: String) {
        if self.state.borrow().switching.is_none() {
            return;
        }
        let trusted = AuthorizedClients::load().and_then(|mut store| {
            store.upsert(AuthorizedClient { name: client_name, fingerprint: client_fingerprint, paired_at: ac_remote::unix_now() });
            store.save()
        });
        if let Err(e) = trusted {
            return self.switch_failed(&format!("couldn't let {} in: {e:#}", self.host.name));
        }
        self.send(ClientMsg::SwitchNow);
        let mut text = format!("Switched sides: {} is connecting to this PC", self.host.name);
        if !hostctl::uinput_writable() {
            text.push_str(". Finish setup in the PC tab so it can use the keyboard and mouse");
        }
        self.tab.viewer_message(&text);
        self.close();
    }

    fn switch_failed(&self, why: &str) {
        if self.state.borrow_mut().switching.take().is_none() {
            return;
        }
        self.show_status("", "");
        self.switch_button.set_sensitive(self.state.borrow().connected);
        self.tab.viewer_message(&format!("Couldn't switch sides: {why}"));
    }

    /// A short message over the picture.
    /// Queues files for the PC; `drop_at` is where on its screen they were dropped.
    fn send_files(&self, paths: Vec<std::path::PathBuf>, drop_at: Option<(f32, f32)>) {
        if !self.state.borrow().connected {
            self.tab.viewer_message(&format!("Connect to {} to send files", self.host.name));
            return;
        }
        for path in paths {
            if path.is_dir() {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.tab.viewer_message(&format!("{name} is a folder: only files can be sent"));
                continue;
            }
            self.state.borrow_mut().files_pending += 1;
            let _ = self.net.tx.send(Outgoing::SendFile { path, drop_at });
        }
        if self.state.borrow().files_pending > 0 && !self.transfer.is_visible() {
            self.transfer.set_label("Sending…");
            self.transfer.set_visible(true);
        }
    }

    fn file_done(&self) {
        let left = {
            let mut s = self.state.borrow_mut();
            s.files_pending = s.files_pending.saturating_sub(1);
            s.files_pending
        };
        if left == 0 {
            self.transfer.set_visible(false);
        }
    }

    fn choose_files(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title(format!("Send files to {}", self.host.name)).accept_label("Send").modal(true).build();
        let weak = Rc::downgrade(self);
        dialog.open_multiple(Some(&self.window), None::<&gtk::gio::Cancellable>, move |res| {
            let (Some(v), Ok(list)) = (weak.upgrade(), res) else { return };
            let paths: Vec<_> = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gtk::gio::File>())
                .filter_map(|f| f.path())
                .collect();
            v.send_files(paths, None);
        });
    }

    /// Files dropped on the PC's screen: the PC saves them in the folder
    /// open under the drop (Windows Explorer), or in Downloads.
    fn attach_drop(self: &Rc<Self>) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        target.connect_drop(move |_, value, x, y| {
            let (Some(v), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else { return false };
            let paths: Vec<_> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            let at = v.to_stream(x, y);
            v.send_files(paths, at);
            true
        });
        self.picture.add_controller(target);
    }

    fn flash(&self, text: &str) {
        if !self.overlay_box.is_visible() {
            self.show_status(text, "");
            let weak = self.overlay_status.downgrade();
            let shown = text.to_string();
            let panel = self.overlay_box.downgrade();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if let (Some(l), Some(p)) = (weak.upgrade(), panel.upgrade())
                    && l.label() == shown
                {
                    p.set_visible(false);
                }
            });
        }
    }

    /// One button per remote monitor, numbered in reading order.
    fn rebuild_monitors(self: &Rc<Self>) {
        while let Some(c) = self.monitors_box.first_child() {
            self.monitors_box.remove(&c);
        }
        let s = self.state.borrow();
        if s.monitors.len() < 2 {
            return;
        }
        for (i, m) in s.monitors.iter().enumerate() {
            let b = gtk::Button::builder()
                .label((i + 1).to_string())
                .tooltip_text(format!("{} ({}×{}) · Ctrl+Alt+Shift+{}", m.name, m.width, m.height, i + 1))
                .build();
            if m.name == s.monitor {
                b.add_css_class("suggested-action");
            }
            let (weak, name) = (Rc::downgrade(self), m.name.clone());
            b.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.switch_monitor(&name);
                }
            });
            self.monitors_box.append(&b);
        }
    }

    fn switch_monitor(&self, name: &str) {
        if self.state.borrow().monitor == name {
            return;
        }
        self.flash(&format!("Switching to {name}…"));
        self.send(ClientMsg::UpdateSettings(StreamSettings { monitor: Some(name.to_string()), ..Default::default() }));
    }

    pub fn toggle_fullscreen(&self) {
        let on = !self.state.borrow().fullscreen;
        self.state.borrow_mut().fullscreen = on;
        if on {
            self.window.fullscreen();
        } else {
            self.window.unfullscreen();
        }
        self.fullscreen_button.set_icon_name(if on { "view-restore-symbolic" } else { "view-fullscreen-symbolic" });
        self.tab.viewer_fullscreen(on);
    }

    fn present_frame(&self) {
        let seq = self.mailbox.seq();
        if seq == self.state.borrow().seq {
            self.report_stats();
            return;
        }
        self.state.borrow_mut().seq = seq;
        let Some(frame) = self.mailbox.take() else { return };
        let (dmabuf, captured) = (frame.is_dmabuf(), frame.pts_us);
        match texture::build(&WidgetExt::display(&self.picture), frame) {
            Ok(t) => {
                self.picture.set_paintable(Some(&t));
                let mut s = self.state.borrow_mut();
                s.frames += 1;
                if let Some(off) = s.offset_us {
                    let lat = ac_remote::clock::now_us() as i64 - (captured as i64 - off);
                    if (0..5_000_000).contains(&lat) {
                        s.latency_us += lat as u64;
                        s.latency_n += 1;
                    }
                }
            }
            Err(e) if dmabuf => {
                if let Some(m) = &self.media {
                    m.disable_dmabuf(&e.to_string());
                }
            }
            Err(e) => tracing::warn!("remote {}: could not build a texture: {e}", self.host.name),
        }
        self.report_stats();
    }

    fn report_stats(&self) {
        let mut s = self.state.borrow_mut();
        let at = *s.stats_at.get_or_insert_with(Instant::now);
        let dt = at.elapsed().as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let text = if s.connected {
            let lat = if s.latency_n > 0 { format!("{:.0} ms", s.latency_us as f64 / s.latency_n as f64 / 1000.0) } else { "– ms".into() };
            format!("{lat} · {:.0} fps · {}×{}", s.frames as f64 / dt, s.stream.0, s.stream.1)
        } else {
            String::new()
        };
        self.stats.set_label(&text);
        self.stats.set_tooltip_text(Some(&format!(
            "Capture to screen: {}\nNetwork round trip: {:.1} ms",
            if s.latency_n > 0 { format!("{:.1} ms", s.latency_us as f64 / s.latency_n as f64 / 1000.0) } else { "unknown".into() },
            s.rtt_us as f64 / 1000.0
        )));
        s.frames = 0;
        s.latency_us = 0;
        s.latency_n = 0;
        s.stats_at = Some(Instant::now());
    }

    /// Widget position → normalised position on the remote screen.
    fn to_stream(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let (fw, fh) = self.mailbox.size()?;
        let lb = Letterbox::new(self.picture.width() as f64, self.picture.height() as f64, fw, fh)?;
        let (rx, ry, rw, rh) = lb.rect();
        Some((((x - rx) / rw).clamp(0.0, 1.0) as f32, ((y - ry) / rh).clamp(0.0, 1.0) as f32))
    }

    fn send_motion(&self, x: f64, y: f64) {
        let Some((nx, ny)) = self.to_stream(x, y) else { return };
        let seq = {
            let mut s = self.state.borrow_mut();
            s.motion_seq = s.motion_seq.wrapping_add(1);
            s.motion_seq
        };
        let _ = self.net.tx.send(Outgoing::Motion(Motion::Abs { seq, x: nx, y: ny }));
    }

    /// An event's position in the picture's coordinates.
    fn local(&self, ev: &gdk::Event) -> Option<(f64, f64)> {
        let (x, y) = ev.position()?;
        let native = self.picture.native()?;
        let (tx, ty) = native.surface_transform();
        let p = native.compute_point(&self.picture, &gtk::graphene::Point::new((x - tx) as f32, (y - ty) as f32))?;
        Some((p.x() as f64, p.y() as f64))
    }

    /// Raw pointer events, so a drag reaches the PC whole even when it
    /// leaves the picture (the press grabs the pointer for this widget).
    fn on_pointer(self: &Rc<Self>, ev: &gdk::Event) -> glib::Propagation {
        match ev.event_type() {
            gdk::EventType::MotionNotify => {
                if self.active()
                    && let Some((x, y)) = self.local(ev)
                {
                    self.send_motion(x, y);
                }
                glib::Propagation::Proceed
            }
            gdk::EventType::ButtonPress => {
                let Some(button) = ev.downcast_ref::<gdk::ButtonEvent>().map(|b| b.button()) else { return glib::Propagation::Proceed };
                self.picture.grab_focus();
                let (connected, forwarding) = {
                    let s = self.state.borrow();
                    (s.connected, s.forwarding)
                };
                if connected && !forwarding {
                    // Click to take back control; the click itself stays here.
                    self.state.borrow_mut().swallowed_buttons.insert(button);
                    self.set_forwarding(true);
                    return glib::Propagation::Stop;
                }
                if !self.active() {
                    return glib::Propagation::Proceed;
                }
                if let Some((x, y)) = self.local(ev) {
                    self.send_motion(x, y);
                }
                if let Some(code) = evdev_button(button) {
                    self.state.borrow_mut().buttons.insert(code);
                    self.send(ClientMsg::Button { code, pressed: true });
                }
                glib::Propagation::Stop
            }
            gdk::EventType::ButtonRelease => {
                let Some(button) = ev.downcast_ref::<gdk::ButtonEvent>().map(|b| b.button()) else { return glib::Propagation::Proceed };
                if self.state.borrow_mut().swallowed_buttons.remove(&button) {
                    return glib::Propagation::Stop;
                }
                // Sent even if control was released meanwhile: the PC saw the press.
                if let Some(code) = evdev_button(button)
                    && self.state.borrow_mut().buttons.remove(&code)
                {
                    self.send(ClientMsg::Button { code, pressed: false });
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn attach_pointer(self: &Rc<Self>) {
        let w = &self.picture;
        let legacy = gtk::EventControllerLegacy::new();
        {
            let weak = Rc::downgrade(self);
            legacy.connect_event(move |_, ev| match weak.upgrade() {
                Some(v) => v.on_pointer(ev),
                None => glib::Propagation::Proceed,
            });
        }
        w.add_controller(legacy);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        {
            let weak = Rc::downgrade(self);
            scroll.connect_scroll(move |c, dx, dy| {
                let Some(v) = weak.upgrade() else { return glib::Propagation::Proceed };
                if !v.active() {
                    return glib::Propagation::Proceed;
                }
                let per_notch = if c.unit() == gdk::ScrollUnit::Wheel { 1.0 } else { PIXELS_PER_NOTCH };
                let (ix, iy) = {
                    let mut s = v.state.borrow_mut();
                    s.wheel.0 += dx / per_notch * 120.0;
                    s.wheel.1 += dy / per_notch * 120.0;
                    let (ix, iy) = (s.wheel.0.trunc(), s.wheel.1.trunc());
                    s.wheel.0 -= ix;
                    s.wheel.1 -= iy;
                    (ix as i32, iy as i32)
                };
                if ix != 0 || iy != 0 {
                    v.send(ClientMsg::Scroll { v120_x: ix, v120_y: iy });
                }
                glib::Propagation::Stop
            });
        }
        w.add_controller(scroll);
    }

    /// Keys are caught on the window, before Test Bed's own shortcuts, and
    /// sent as evdev codes: the PC applies its own keyboard layout.
    fn attach_keys(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let weak = Rc::downgrade(self);
            keys.connect_key_pressed(move |_, keyval, keycode, state| {
                let Some(v) = weak.upgrade() else { return glib::Propagation::Proceed };
                if !v.root.is_mapped() {
                    return glib::Propagation::Proceed;
                }
                if state.contains(CHORD) && v.chord(keyval) {
                    v.state.borrow_mut().swallowed.insert(keycode);
                    return glib::Propagation::Stop;
                }
                let Some(code) = evdev_from_gtk(keycode).filter(|_| v.active()) else {
                    return glib::Propagation::Proceed;
                };
                // Held keys repeat on the PC itself.
                if v.state.borrow_mut().keys.insert(code) {
                    v.send(ClientMsg::Key { code, pressed: true });
                }
                glib::Propagation::Stop
            });
        }
        {
            let weak = Rc::downgrade(self);
            keys.connect_key_released(move |_, _, keycode, _| {
                let Some(v) = weak.upgrade() else { return };
                if v.state.borrow_mut().swallowed.remove(&keycode) {
                    return;
                }
                let Some(code) = evdev_from_gtk(keycode) else { return };
                if v.state.borrow_mut().keys.remove(&code) {
                    v.send(ClientMsg::Key { code, pressed: false });
                }
            });
        }
        self.window.add_controller(keys.clone());
        *self.keys.borrow_mut() = Some(keys);

        let weak = Rc::downgrade(self);
        let id = self.window.connect_is_active_notify(move |w| {
            let Some(v) = weak.upgrade() else { return };
            if !w.is_active() {
                // Keys held when focus left would stay down on the PC.
                v.release_all();
                v.state.borrow_mut().swallowed.clear();
            }
            v.update_grab();
        });
        *self.window_active.borrow_mut() = Some(id);
    }

    /// Ctrl+Alt+Shift chords; true if handled.
    fn chord(self: &Rc<Self>, keyval: gdk::Key) -> bool {
        let Some(c) = keyval.to_lower().to_unicode() else { return false };
        match c {
            'q' => {
                let f = !self.state.borrow().forwarding;
                self.set_forwarding(f);
            }
            'f' => self.toggle_fullscreen(),
            's' => self.switch_sides(),
            'd' => {
                self.close();
                return true;
            }
            '1'..='9' => {
                let name = self.state.borrow().monitors.get(c as usize - '1' as usize).map(|m| m.name.clone());
                match name {
                    Some(n) => self.switch_monitor(&n),
                    None => return false,
                }
            }
            _ => return false,
        }
        // The chord's modifiers already reached the PC: let go of them there.
        if self.state.borrow().connected {
            self.release_all();
        }
        true
    }

    /// Disconnects and hands the tab back to the list of computers.
    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        if self.state.borrow().connected {
            self.release_all();
        }
        if self.state.borrow().fullscreen {
            self.toggle_fullscreen();
        }
        self.update_grab();
        let _ = self.net.tx.send(Outgoing::Quit);
        self.net.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(k) = self.keys.take() {
            self.window.remove_controller(&k);
        }
        if let Some(id) = self.window_active.take() {
            self.window.disconnect(id);
        }
        self.tab.viewer_closed();
    }
}

fn evdev_button(gtk_button: u32) -> Option<u16> {
    Some(match gtk_button {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x113, // BTN_SIDE
        9 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

fn open_decoder(media: Option<&MediaContext>, backend: &mut Backend) -> Option<Decoder> {
    if *backend != Backend::Software
        && let Some(hw) = media.and_then(|m| m.hw_device())
    {
        match Decoder::new(VideoCodec::H264, Some(hw)) {
            Ok(d) => return Some(d),
            Err(e) => tracing::warn!("remote: VA-API decoder unavailable ({e}); using software"),
        }
    }
    *backend = Backend::Software;
    Decoder::new(VideoCodec::H264, None).map_err(|e| tracing::error!("remote: no H.264 decoder: {e}")).ok()
}

/// Decodes frames as they arrive; the newest one waits in the mailbox.
fn decode_loop(rx: mpsc::Receiver<(FrameMeta, Vec<u8>)>, mailbox: &Mailbox, media: Option<&MediaContext>, net: &tokio::sync::mpsc::UnboundedSender<Outgoing>) {
    let mut backend = media.map_or(Backend::Software, |m| m.backend());
    let Some(mut decoder) = open_decoder(media, &mut backend) else { return };
    let mut errors: VecDeque<Instant> = VecDeque::new();
    let mut last_key_request = Instant::now() - KEYFRAME_MIN_INTERVAL;
    for (meta, data) in rx {
        if backend == Backend::VaapiDmabuf && media.is_some_and(|m| m.backend() != Backend::VaapiDmabuf) {
            backend = Backend::VaapiCopy;
        }
        let out = if backend == Backend::VaapiDmabuf { HwOutput::Dmabuf } else { HwOutput::Copy };
        match decoder.decode(&data, meta.capture_us, meta.keyframe, out) {
            Ok(Some(frame)) => mailbox.put(frame),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("remote decode error: {e}");
                let now = Instant::now();
                errors.push_back(now);
                while errors.front().is_some_and(|t| now.duration_since(*t) > ERROR_WINDOW) {
                    errors.pop_front();
                }
                if last_key_request.elapsed() >= KEYFRAME_MIN_INTERVAL {
                    last_key_request = now;
                    let _ = net.send(Outgoing::Msg(ClientMsg::RequestKeyframe));
                }
                if errors.len() >= ERROR_BURST && decoder.is_hw() {
                    tracing::warn!("remote: hardware decoding keeps failing ({e}); switching to software");
                    backend = Backend::Software;
                    let Some(d) = open_decoder(None, &mut backend) else { return };
                    decoder = d;
                    errors.clear();
                }
            }
        }
    }
}
