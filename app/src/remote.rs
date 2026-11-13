//! The PC tab: share this PC with paired computers, and see and control
//! other PCs on the local network. Both sides run Test Bed; pairing is a
//! one-time 6-digit code. PCs on other networks are a separate list, paired
//! with an invite code and reached over the internet (spec3).
//! Ported from RemoteLinux, which Test Bed replaces:
//! its pairings come along, and its host service is switched off.
//!
//! Sharing runs as the `test-bed-share` user service, so this PC can be
//! reached from login on, whether or not Test Bed is open.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ac_media::MediaContext;
use ac_remote::config::ClientConfig;
use ac_remote::discovery::{self, Discovered};
use ac_remote::hostctl::{self, HostStatus, PairEvent, PairWindow};
use ac_remote::trust::{KnownHost, KnownHosts};
use adw::prelude::*;
use gtk::glib;

use crate::remote_viewer::{Viewer, ViewerHost};

const REFRESH_EVERY: Duration = Duration::from_secs(4);
const DISCOVER_FOR: Duration = Duration::from_millis(1500);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// PCs on other networks: each check is a handshake through a relay.
const INTERNET_REFRESH_EVERY: Duration = Duration::from_secs(20);
const INTERNET_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The tab's own runtime for its network calls (the viewer's connection
/// runs one of its own, like RemoteLinux's).
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("remote-tab")
            .enable_all()
            .build()
            .expect("failed to start the remote tab's runtime")
    })
}

/// Runs `fut` off the main thread and hands its result to `done` on it.
fn run<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static, done: impl FnOnce(T) + 'static) {
    let (tx, rx) = async_channel::bounded(1);
    rt().spawn(async move {
        let _ = tx.send(fut.await).await;
    });
    glib::spawn_future_local(async move {
        if let Ok(v) = rx.recv().await {
            done(v);
        }
    });
}

/// Like [`run`] for blocking work (systemctl, pkexec).
fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    run(async move { tokio::task::spawn_blocking(f).await.expect("blocking task panicked") }, done);
}

/// Shares this PC, turning sharing on if it was off, and reports the
/// running share service. Switching sides needs it.
pub fn share_now(done: impl FnOnce(anyhow::Result<HostStatus>) + 'static) {
    run(
        async {
            if let Some(s) = hostctl::status().await {
                return Ok(s);
            }
            tokio::task::spawn_blocking(|| {
                let bin = hostctl::host_binary().ok_or_else(|| anyhow::anyhow!("test-bed-share is not installed next to Test Bed"))?;
                if hostctl::remotelinux_enabled() {
                    hostctl::retire_remotelinux()?;
                }
                hostctl::enable_service(&bin)
            })
            .await??;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(s) = hostctl::status().await {
                    return Ok(s);
                }
            }
            anyhow::bail!("sharing didn't start")
        },
        done,
    );
}

/// What one refresh found.
struct Snapshot {
    status: Option<HostStatus>,
    service_enabled: bool,
    needs_uinput: bool,
    needs_firewall: bool,
    found: Vec<Discovered>,
    known: Vec<KnownHost>,
    /// Fingerprint → round trip in ms, for paired PCs that answered.
    online: HashMap<String, f64>,
    /// Access from other networks is turned on (`host.toml`).
    internet_enabled: bool,
}

async fn snapshot() -> Snapshot {
    let status = hostctl::status().await;
    let found = discovery::discover(DISCOVER_FOR).await.unwrap_or_else(|e| {
        tracing::debug!("remote discovery: {e:#}");
        Vec::new()
    });
    let mut kh = KnownHosts::load().unwrap_or_default();
    // A paired PC found at a new address (DHCP): remember where it is now.
    let mut moved = false;
    for d in &found {
        if let Some(h) = kh.hosts.iter_mut().find(|h| h.fingerprint == d.fingerprint)
            && h.address != d.address
            && h.address.parse::<std::net::SocketAddr>().is_ok()
        {
            h.address = d.address.clone();
            moved = true;
        }
    }
    if moved {
        let _ = kh.save();
    }
    let probes = ac_remote::client::probe(&kh.hosts, PROBE_TIMEOUT).await;
    let online = kh
        .hosts
        .iter()
        .zip(probes)
        .filter_map(|(h, p)| p.online.then(|| (h.fingerprint.clone(), p.rtt_ms.unwrap_or(0.0))))
        .collect();
    let (service_enabled, needs_uinput, needs_firewall) = tokio::task::spawn_blocking(|| {
        (hostctl::service_enabled(), !hostctl::uinput_writable(), hostctl::ufw_active() && !hostctl::firewall_opened())
    })
    .await
    .unwrap_or((false, false, false));
    let internet_enabled = ac_remote::config::HostConfig::load().map(|c| c.internet).unwrap_or(false);
    Snapshot { status, service_enabled, needs_uinput, needs_firewall, found, known: kh.hosts, online, internet_enabled }
}

pub struct RemoteTab {
    pub root: gtk::Stack,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    toolbar: adw::ToolbarView,
    media: Option<Arc<MediaContext>>,
    offload: bool,
    share: adw::SwitchRow,
    setup: adw::ActionRow,
    pair: adw::ActionRow,
    internet: adw::SwitchRow,
    invite: adw::ActionRow,
    remote: adw::PreferencesGroup,
    remote_rows: RefCell<Vec<gtk::Widget>>,
    remote_refreshing: Cell<bool>,
    paired: adw::PreferencesGroup,
    paired_rows: RefCell<Vec<gtk::Widget>>,
    computers: adw::PreferencesGroup,
    computer_rows: RefCell<Vec<gtk::Widget>>,
    refresh_button: gtk::Button,
    viewer_slot: gtk::Box,
    viewer: RefCell<Option<Rc<Viewer>>>,
    /// Set while the switch is moved by a refresh, not the user.
    syncing: Cell<bool>,
    refreshing: Cell<bool>,
    busy: Cell<bool>,
    grabbing: Cell<bool>,
    /// Application accelerators taken away while a remote PC has the keyboard.
    saved_accels: RefCell<Vec<(String, Vec<glib::GString>)>>,
    own_fingerprint: RefCell<Option<String>>,
}

impl RemoteTab {
    pub fn new(
        window: &adw::ApplicationWindow,
        toasts: &adw::ToastOverlay,
        toolbar: &adw::ToolbarView,
        media: Option<Arc<MediaContext>>,
        offload: bool,
    ) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        let share_group = adw::PreferencesGroup::builder()
            .title("Share this PC")
            .description("Paired computers on your network can see and control this PC. Sharing starts at login, even when Test Bed is closed.")
            .build();
        let share = adw::SwitchRow::builder().title("Share this PC").subtitle("Off").build();
        let setup_button = gtk::Button::builder().label("Allow").valign(gtk::Align::Center).build();
        setup_button.add_css_class("suggested-action");
        let setup = adw::ActionRow::builder().title("Finish setup").visible(false).build();
        setup.add_suffix(&setup_button);
        let pair_button = gtk::Button::builder().label("Show pairing code").valign(gtk::Align::Center).build();
        let pair = adw::ActionRow::builder()
            .title("Pair a computer")
            .subtitle("Shows a code to type in Test Bed on the other computer")
            .sensitive(false)
            .build();
        pair.add_suffix(&pair_button);
        let internet = adw::SwitchRow::builder().title("Allow access from other networks").subtitle("Off").build();
        let invite_button = gtk::Button::builder().label("Show invite code").valign(gtk::Align::Center).build();
        let invite = adw::ActionRow::builder()
            .title("Invite a computer on another network")
            .subtitle("Shows a code to send to the other computer")
            .sensitive(false)
            .build();
        invite.add_suffix(&invite_button);
        share_group.add(&share);
        share_group.add(&setup);
        share_group.add(&pair);
        share_group.add(&internet);
        share_group.add(&invite);

        let paired = adw::PreferencesGroup::builder().title("Computers that can control this PC").visible(false).build();

        let refresh_button = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Look again").valign(gtk::Align::Center).build();
        refresh_button.add_css_class("flat");
        let computers = adw::PreferencesGroup::builder()
            .title("Computers")
            .description("PCs on your network that share themselves with Test Bed")
            .header_suffix(&refresh_button)
            .build();

        let remote_refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Check again").valign(gtk::Align::Center).build();
        remote_refresh.add_css_class("flat");
        let remote = adw::PreferencesGroup::builder()
            .title("Computers on other networks")
            .description("PCs paired with an invite code, reached over the internet")
            .header_suffix(&remote_refresh)
            .build();

        page.add(&share_group);
        page.add(&paired);
        page.add(&computers);
        page.add(&remote);

        let viewer_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let root = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).build();
        root.add_named(&page, Some("list"));
        root.add_named(&viewer_slot, Some("viewer"));

        let tab = Rc::new(Self {
            root,
            window: window.clone(),
            toasts: toasts.clone(),
            toolbar: toolbar.clone(),
            media,
            offload,
            share,
            setup,
            pair,
            internet,
            invite,
            remote,
            remote_rows: RefCell::default(),
            remote_refreshing: Cell::new(false),
            paired,
            paired_rows: RefCell::default(),
            computers,
            computer_rows: RefCell::default(),
            refresh_button,
            viewer_slot,
            viewer: RefCell::default(),
            syncing: Cell::new(false),
            refreshing: Cell::new(false),
            busy: Cell::new(false),
            grabbing: Cell::new(false),
            saved_accels: RefCell::default(),
            own_fingerprint: RefCell::default(),
        });

        {
            let weak = Rc::downgrade(&tab);
            tab.share.connect_active_notify(move |row| {
                if let Some(t) = weak.upgrade()
                    && !t.syncing.get()
                {
                    t.set_sharing(row.is_active());
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            setup_button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.finish_setup();
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            pair_button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.show_pairing_code(false);
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            invite_button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.show_pairing_code(true);
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            tab.internet.connect_active_notify(move |row| {
                if let Some(t) = weak.upgrade()
                    && !t.syncing.get()
                {
                    t.set_internet(row.is_active());
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            remote_refresh.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.refresh_remote();
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            glib::timeout_add_local(INTERNET_REFRESH_EVERY, move || match weak.upgrade() {
                Some(t) => {
                    if t.root.is_mapped() && t.root.visible_child_name().as_deref() == Some("list") {
                        t.refresh_remote();
                    }
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            tab.refresh_button.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.refresh();
                }
            });
        }
        {
            // Look around while the list is on screen.
            let weak = Rc::downgrade(&tab);
            glib::timeout_add_local(REFRESH_EVERY, move || match weak.upgrade() {
                Some(t) => {
                    if t.root.is_mapped() && t.root.visible_child_name().as_deref() == Some("list") {
                        t.refresh();
                    }
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            tab.root.connect_map(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.refresh();
                    t.refresh_remote();
                }
            });
        }
        tab.render_computers(&[], &[], &HashMap::new());
        tab.render_remote(&[], None);
        tab.take_over_from_remotelinux();
        tab
    }

    fn toast(&self, msg: &str) {
        let t = adw::Toast::new(msg);
        t.set_timeout(6);
        self.toasts.add_toast(t);
    }

    /// Test Bed replaces RemoteLinux: bring its pairings over once, and if
    /// it was sharing this PC, share with Test Bed's service instead.
    fn take_over_from_remotelinux(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        blocking(
            || {
                let migrated = ac_remote::paths::migrate_from_remotelinux();
                if !hostctl::remotelinux_enabled() {
                    // Sharing on, but its binary moved (Test Bed reinstalled): point the service at this one.
                    if hostctl::service_enabled()
                        && hostctl::service_binary().is_some_and(|b| !b.exists())
                        && let Some(bin) = hostctl::host_binary()
                    {
                        hostctl::enable_service(&bin)?;
                    }
                    return Ok((migrated, false));
                }
                let bin = hostctl::host_binary().ok_or_else(|| anyhow::anyhow!("test-bed-share is not installed"))?;
                hostctl::retire_remotelinux()?;
                hostctl::enable_service(&bin)?;
                Ok::<_, anyhow::Error>((migrated, true))
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                match r {
                    Ok((_, true)) => t.toast("Sharing moved from RemoteLinux to Test Bed"),
                    Ok((true, false)) => t.toast("Brought over your RemoteLinux pairings"),
                    Ok(_) => {}
                    Err(e) => t.toast(&format!("Couldn't take over sharing from RemoteLinux: {e:#}")),
                }
                t.refresh();
            },
        );
    }

    pub fn refresh(self: &Rc<Self>) {
        if self.refreshing.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        run(snapshot(), move |s| {
            if let Some(t) = weak.upgrade() {
                t.refreshing.set(false);
                t.apply(s);
            }
        });
    }

    fn apply(self: &Rc<Self>, s: Snapshot) {
        let running = s.status.is_some();
        if !self.busy.get() {
            self.syncing.set(true);
            self.share.set_active(s.service_enabled || running);
            self.internet.set_active(s.internet_enabled);
            self.syncing.set(false);
        }
        let net = s.status.as_ref().map(|st| &st.internet);
        let internet_subtitle = match net {
            _ if !s.internet_enabled => "Off · paired computers on other networks can't reach this PC".to_string(),
            None => "Starts with sharing".to_string(),
            Some(n) if n.online => format!("On · reachable through {}", relay_host(n.relay.as_deref())),
            Some(n) => match n.error.as_deref() {
                None | Some("starting") => "Connecting to the relay…".to_string(),
                Some(e) => format!("Can't reach the relay: {e}"),
            },
        };
        self.internet.set_subtitle(&internet_subtitle);
        self.invite.set_sensitive(net.is_some_and(|n| n.online));
        let subtitle = match &s.status {
            None if s.service_enabled => "Starting…".to_string(),
            None => "Off".to_string(),
            Some(st) => {
                let ip = discovery::local_ip().map(|ip| format!(" · {ip}")).unwrap_or_default();
                match &st.client {
                    Some(c) => format!("{} is connected · {}×{} · {} fps", c.name, c.width, c.height, c.fps),
                    None => format!("On · {}{ip}", st.name),
                }
            }
        };
        self.share.set_subtitle(&subtitle);
        self.pair.set_sensitive(running);
        let mut needs = Vec::new();
        if s.needs_uinput {
            needs.push("lets paired computers use the keyboard and mouse");
        }
        if s.needs_firewall {
            needs.push("opens the firewall to your home network");
        }
        self.setup.set_visible(running && !needs.is_empty());
        if !needs.is_empty() {
            let text = needs.join(" and ");
            self.setup.set_subtitle(&format!("{}{}. Asks for your password once.", text[..1].to_uppercase(), &text[1..]));
        }
        *self.own_fingerprint.borrow_mut() = s.status.as_ref().map(|st| st.fingerprint.clone());
        self.render_paired(s.status.as_ref());
        self.render_computers(&s.found, &s.known, &s.online);
    }

    fn render_paired(self: &Rc<Self>, status: Option<&HostStatus>) {
        for w in self.paired_rows.take() {
            self.paired.remove(&w);
        }
        let Some(st) = status else {
            self.paired.set_visible(false);
            return;
        };
        self.paired.set_visible(!st.paired.is_empty());
        for c in &st.paired {
            let connected = st.client.as_ref().is_some_and(|cc| cc.name == c.name);
            let place = if c.internet { "Other network · " } else { "" };
            let row = adw::ActionRow::builder()
                .title(&c.name)
                .subtitle(if connected { format!("Connected · {place}{}", c.fingerprint_short) } else { format!("{place}{}", c.fingerprint_short) })
                .build();
            if connected {
                let b = gtk::Button::builder().label("Disconnect").valign(gtk::Align::Center).build();
                let weak = Rc::downgrade(self);
                b.connect_clicked(move |_| {
                    let Some(t) = weak.upgrade() else { return };
                    let w2 = Rc::downgrade(&t);
                    run(hostctl::disconnect_client(), move |_| {
                        if let Some(t) = w2.upgrade() {
                            t.refresh();
                        }
                    });
                });
                row.add_suffix(&b);
            }
            let unpair = gtk::Button::builder().label("Unpair").valign(gtk::Align::Center).build();
            unpair.add_css_class("destructive-action");
            {
                let (weak, name) = (Rc::downgrade(self), c.name.clone());
                unpair.connect_clicked(move |_| {
                    let Some(t) = weak.upgrade() else { return };
                    let (w2, n2) = (Rc::downgrade(&t), name.clone());
                    run(async move { hostctl::unpair(&n2).await }, move |r| {
                        let Some(t) = w2.upgrade() else { return };
                        if let Err(e) = r {
                            t.toast(&format!("Couldn't unpair: {e:#}"));
                        }
                        t.refresh();
                    });
                });
            }
            row.add_suffix(&unpair);
            self.paired.add(&row);
            self.paired_rows.borrow_mut().push(row.upcast());
        }
    }

    fn render_computers(self: &Rc<Self>, found: &[Discovered], known: &[KnownHost], online: &HashMap<String, f64>) {
        for w in self.computer_rows.take() {
            self.computers.remove(&w);
        }
        let own = self.own_fingerprint.borrow().clone();
        let mut known: Vec<&KnownHost> = known.iter().collect();
        known.sort_by_key(|h| (!online.contains_key(&h.fingerprint), std::cmp::Reverse(h.last_seen.unwrap_or(h.paired_at))));
        for h in known {
            let rtt = online.get(&h.fingerprint);
            let subtitle = match rtt {
                Some(ms) => format!("Online · {ms:.0} ms · {}", h.address),
                None => format!("Offline · {}", h.address),
            };
            let row = adw::ActionRow::builder().title(&h.name).subtitle(subtitle).build();
            let connect = gtk::Button::builder().label("Connect").valign(gtk::Align::Center).sensitive(rtt.is_some()).build();
            connect.add_css_class("suggested-action");
            {
                let (weak, host) = (Rc::downgrade(self), h.clone());
                connect.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.connect(host.clone());
                    }
                });
            }
            let forget = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Forget this PC").valign(gtk::Align::Center).build();
            forget.add_css_class("flat");
            {
                let (weak, name) = (Rc::downgrade(self), h.name.clone());
                forget.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.forget(&name);
                    }
                });
            }
            row.add_suffix(&connect);
            row.add_suffix(&forget);
            self.computers.add(&row);
            self.computer_rows.borrow_mut().push(row.upcast());
        }
        for d in found {
            if Some(&d.fingerprint) == own.as_ref() || online.contains_key(&d.fingerprint) {
                continue;
            }
            let subtitle = if d.pairing { format!("Showing a pairing code · {}", d.address) } else { format!("Not paired · {}", d.address) };
            let row = adw::ActionRow::builder().title(&d.name).subtitle(subtitle).build();
            let b = gtk::Button::builder().label("Pair").valign(gtk::Align::Center).build();
            if d.pairing {
                b.add_css_class("suggested-action");
            }
            {
                let (weak, d) = (Rc::downgrade(self), d.clone());
                b.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.ask_pair(Some(&d));
                    }
                });
            }
            row.add_suffix(&b);
            self.computers.add(&row);
            self.computer_rows.borrow_mut().push(row.upcast());
        }
        let by_address = adw::ActionRow::builder()
            .title("Pair by address")
            .subtitle("For a PC that doesn't show up here")
            .activatable(true)
            .build();
        by_address.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let weak = Rc::downgrade(self);
            by_address.connect_activated(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.ask_pair(None);
                }
            });
        }
        self.computers.add(&by_address);
        self.computer_rows.borrow_mut().push(by_address.upcast());
    }

    fn set_sharing(self: &Rc<Self>, on: bool) {
        self.busy.set(true);
        self.share.set_subtitle(if on { "Turning on…" } else { "Turning off…" });
        let weak = Rc::downgrade(self);
        blocking(
            move || {
                if on {
                    let bin = hostctl::host_binary().ok_or_else(|| anyhow::anyhow!("test-bed-share is not installed next to Test Bed"))?;
                    if hostctl::remotelinux_enabled() {
                        hostctl::retire_remotelinux()?;
                    }
                    hostctl::enable_service(&bin)
                } else {
                    hostctl::disable_service()
                }
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                t.busy.set(false);
                if let Err(e) = r {
                    t.toast(&format!("Couldn't turn sharing {}: {e:#}", if on { "on" } else { "off" }));
                }
                // The service needs a moment to open its control socket.
                let w2 = Rc::downgrade(&t);
                glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    if let Some(t) = w2.upgrade() {
                        t.refresh();
                    }
                });
            },
        );
    }

    fn finish_setup(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        blocking(
            || {
                let mut script = Vec::new();
                if !hostctl::uinput_writable() {
                    hostctl::fix_uinput()?;
                    script.push("input");
                }
                if hostctl::ufw_active() && !hostctl::firewall_opened() {
                    hostctl::open_firewall()?;
                    script.push("firewall");
                }
                // The service opened its input devices at start: restart it.
                if script.contains(&"input") && hostctl::service_enabled() {
                    let _ = std::process::Command::new("systemctl").args(["--user", "restart", hostctl::SERVICE]).status();
                }
                Ok::<_, anyhow::Error>(())
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                match r {
                    Ok(()) => t.toast("Setup finished"),
                    Err(e) => t.toast(&format!("Setup didn't finish: {e:#}")),
                }
                t.refresh();
            },
        );
    }

    /// Shows a code for another PC to pair with this one: a 6-digit PIN on
    /// the LAN, or an invite code to send to a PC on another network.
    fn show_pairing_code(self: &Rc<Self>, invite: bool) {
        let alert = adw::AlertDialog::new(Some(if invite { "Invite code" } else { "Pairing code" }), Some("Connecting…"));
        let code = gtk::Label::builder().css_classes(if invite { ["monospace", "body"] } else { ["title-1", "numeric"] }).selectable(true).wrap(true).wrap_mode(gtk::pango::WrapMode::Char).build();
        alert.set_extra_child(Some(&code));
        if invite {
            alert.add_response("copy", "Copy");
            alert.set_response_enabled("copy", false);
            alert.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
            let (window, code) = (self.window.clone(), code.clone());
            alert.connect_response(Some("copy"), move |a, _| {
                window.clipboard().set_text(&code.label());
                // Keep the dialog open: it reports when the other PC pairs.
                a.set_body("Copied. Send it to the other computer; this closes when it pairs.");
            });
            alert.set_close_response("cancel");
        }
        alert.add_response("cancel", "Cancel");
        let (tx, rx) = async_channel::unbounded::<Result<(String, Option<String>, u64), String>>();
        let (ev_tx, ev_rx) = async_channel::unbounded::<PairEvent>();
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
        rt().spawn(async move {
            let opened = if invite { PairWindow::open_invite().await } else { PairWindow::open().await };
            let mut win = match opened {
                Ok(w) => w,
                Err(e) => {
                    let _ = tx.send(Err(format!("{e:#}"))).await;
                    return;
                }
            };
            let _ = tx.send(Ok((win.pin.clone(), win.invite.clone(), win.expires_in))).await;
            loop {
                tokio::select! {
                    ev = win.next() => {
                        let end = matches!(ev, PairEvent::Paired { .. } | PairEvent::Expired | PairEvent::Cancelled);
                        let _ = ev_tx.send(ev).await;
                        if end { break; }
                    }
                    // Dropping the window closes it on the host.
                    _ = stop_rx.recv() => break,
                }
            }
        });
        alert.connect_response(None, move |_, r| {
            if r != "copy" {
                let _ = stop_tx.try_send(());
            }
        });
        {
            let (alert, code) = (alert.clone(), code.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok((_, Some(invite), secs))) => {
                        code.set_label(&invite);
                        alert.set_response_enabled("copy", true);
                        alert.set_body(&format!(
                            "Send this code to the other computer. There, open Test Bed's PC tab and click Add with invite code. It works for {} minutes.",
                            secs.div_ceil(60)
                        ));
                    }
                    Ok(Ok((pin, None, secs))) => {
                        code.set_label(&format!("{} {}", &pin[..3], &pin[3..]));
                        alert.set_body(&format!(
                            "On the other computer, open Test Bed's PC tab, pick this PC and type this code. It works for {} minutes.",
                            secs.div_ceil(60)
                        ));
                    }
                    Ok(Err(e)) => alert.set_body(&format!("Couldn't get a code: {e}")),
                    Err(_) => {}
                }
            });
        }
        {
            let (alert, weak) = (alert.clone(), Rc::downgrade(self));
            glib::spawn_future_local(async move {
                while let Ok(ev) = ev_rx.recv().await {
                    let Some(t) = weak.upgrade() else { return };
                    match ev {
                        PairEvent::Paired { name } => {
                            alert.close();
                            t.toast(&format!("{name} can now control this PC"));
                            t.refresh();
                        }
                        PairEvent::WrongPin { attempts_left } if invite => {
                            alert.set_body(&format!("A computer tried this code and failed. {attempts_left} tries left."));
                        }
                        PairEvent::WrongPin { attempts_left } => {
                            alert.set_body(&format!("Wrong code typed. {attempts_left} tries left."));
                        }
                        PairEvent::Expired => alert.set_body("The code expired. Close this and show a new one."),
                        PairEvent::Cancelled => {
                            alert.close();
                        }
                    }
                }
            });
        }
        alert.present(Some(&self.window));
    }

    /// Asks for the code shown on `target` (or its address, then the code).
    fn ask_pair(self: &Rc<Self>, target: Option<&Discovered>) {
        let (heading, body) = match target {
            Some(d) => (format!("Pair with {}", d.name), format!("On {}, open Test Bed's PC tab and click Show pairing code.", d.name)),
            None => ("Pair by address".into(), "On the other PC, open Test Bed's PC tab and click Show pairing code.".into()),
        };
        let alert = adw::AlertDialog::new(Some(&heading), Some(&body));
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let address = gtk::Entry::builder().placeholder_text("Address, e.g. 192.168.0.10").activates_default(true).build();
        if target.is_none() {
            fields.append(&address);
        }
        let code = gtk::Entry::builder().placeholder_text("6-digit code").input_purpose(gtk::InputPurpose::Digits).max_length(7).activates_default(true).build();
        fields.append(&code);
        alert.set_extra_child(Some(&fields));
        alert.add_response("cancel", "Cancel");
        alert.add_response("pair", "Pair");
        alert.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
        alert.set_default_response(Some("pair"));
        let addr = target.map(|d| d.address.clone());
        let weak = Rc::downgrade(self);
        alert.connect_response(Some("pair"), move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            let address = addr.clone().unwrap_or_else(|| address.text().trim().to_string());
            let pin = code.text().to_string();
            if address.is_empty() {
                t.toast("Type the other PC's address");
                return;
            }
            let name = ClientConfig::load().map(|c| c.device_name()).unwrap_or_else(|_| ac_remote::hostname());
            let w2 = Rc::downgrade(&t);
            run(async move { ac_remote::client::pair(&address, &pin, &name).await }, move |r| {
                let Some(t) = w2.upgrade() else { return };
                match r {
                    Ok(h) => t.toast(&format!("Paired with {}", h.name)),
                    Err(e) => t.toast(&format!("Pairing failed: {e:#}")),
                }
                t.refresh();
            });
        });
        alert.present(Some(&self.window));
    }

    fn set_internet(self: &Rc<Self>, on: bool) {
        self.busy.set(true);
        self.internet.set_subtitle(if on { "Turning on…" } else { "Turning off…" });
        let weak = Rc::downgrade(self);
        blocking(
            move || hostctl::set_internet_access(on),
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                t.busy.set(false);
                if let Err(e) = r {
                    t.toast(&format!("Couldn't change access from other networks: {e:#}"));
                }
                // The service restarts and then needs a moment to reach its relay.
                for ms in [800, 3000] {
                    let w2 = Rc::downgrade(&t);
                    glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                        if let Some(t) = w2.upgrade() {
                            t.refresh();
                        }
                    });
                }
            },
        );
    }

    /// Checks which PCs on other networks are online, then lists them.
    fn refresh_remote(self: &Rc<Self>) {
        if self.remote_refreshing.replace(true) {
            return;
        }
        let hosts = KnownHosts::load_internet().map(|k| k.hosts).unwrap_or_default();
        if self.remote_rows.borrow().len() <= 1 {
            self.render_remote(&hosts, None);
        }
        let relay = ClientConfig::load().map(|c| c.relay).unwrap_or_default();
        let weak = Rc::downgrade(self);
        run(
            async move {
                let probes = ac_remote::internet::probe(&hosts, &relay, INTERNET_PROBE_TIMEOUT).await;
                let online: HashMap<String, f64> =
                    hosts.iter().zip(probes).filter_map(|(h, p)| p.online.then(|| (h.fingerprint.clone(), p.rtt_ms.unwrap_or(0.0)))).collect();
                (hosts, online)
            },
            move |(hosts, online)| {
                if let Some(t) = weak.upgrade() {
                    t.remote_refreshing.set(false);
                    t.render_remote(&hosts, Some(&online));
                }
            },
        );
    }

    /// `online` is `None` while the first check runs.
    fn render_remote(self: &Rc<Self>, hosts: &[KnownHost], online: Option<&HashMap<String, f64>>) {
        for w in self.remote_rows.take() {
            self.remote.remove(&w);
        }
        let mut hosts: Vec<&KnownHost> = hosts.iter().collect();
        hosts.sort_by_key(|h| (online.is_some_and(|o| !o.contains_key(&h.fingerprint)), std::cmp::Reverse(h.last_seen.unwrap_or(h.paired_at))));
        for h in hosts {
            let rtt = online.and_then(|o| o.get(&h.fingerprint));
            let subtitle = match (online, rtt) {
                (None, _) => "Checking…".to_string(),
                (_, Some(ms)) => format!("Online · {ms:.0} ms"),
                (_, None) => "Offline, or access from other networks is off there".to_string(),
            };
            let row = adw::ActionRow::builder().title(&h.name).subtitle(subtitle).build();
            let connect = gtk::Button::builder().label("Connect").valign(gtk::Align::Center).sensitive(rtt.is_some()).build();
            connect.add_css_class("suggested-action");
            {
                let (weak, host) = (Rc::downgrade(self), h.clone());
                connect.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.connect(host.clone());
                    }
                });
            }
            let forget = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Forget this PC").valign(gtk::Align::Center).build();
            forget.add_css_class("flat");
            {
                let (weak, id) = (Rc::downgrade(self), h.fingerprint.clone());
                forget.connect_clicked(move |_| {
                    if let Some(t) = weak.upgrade() {
                        t.forget_remote(&id);
                    }
                });
            }
            row.add_suffix(&connect);
            row.add_suffix(&forget);
            self.remote.add(&row);
            self.remote_rows.borrow_mut().push(row.upcast());
        }
        let add = adw::ActionRow::builder()
            .title("Add with invite code")
            .subtitle("On the other PC: Allow access from other networks, then Show invite code")
            .activatable(true)
            .build();
        add.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let weak = Rc::downgrade(self);
            add.connect_activated(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.ask_invite();
                }
            });
        }
        self.remote.add(&add);
        self.remote_rows.borrow_mut().push(add.upcast());
    }

    /// Asks for an invite code from a PC on another network, and pairs.
    fn ask_invite(self: &Rc<Self>) {
        let alert = adw::AlertDialog::new(
            Some("Add with invite code"),
            Some("Paste the invite code from the other computer. It's shown there under Share this PC › Show invite code."),
        );
        let entry = gtk::Entry::builder().placeholder_text("tb1:…").activates_default(true).build();
        alert.set_extra_child(Some(&entry));
        alert.add_response("cancel", "Cancel");
        alert.add_response("pair", "Pair");
        alert.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
        alert.set_default_response(Some("pair"));
        let weak = Rc::downgrade(self);
        alert.connect_response(Some("pair"), move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            let code = entry.text().to_string();
            let cfg = ClientConfig::load().unwrap_or_default();
            let (name, relay) = (cfg.device_name(), cfg.relay);
            t.toast("Pairing over the internet…");
            let w2 = Rc::downgrade(&t);
            run(async move { ac_remote::internet::pair(&code, &name, &relay).await }, move |r| {
                let Some(t) = w2.upgrade() else { return };
                match r {
                    Ok(h) => t.toast(&format!("Paired with {}", h.name)),
                    Err(e) => t.toast(&format!("Pairing failed: {e:#}")),
                }
                t.refresh_remote();
            });
        });
        alert.present(Some(&self.window));
    }

    fn forget_remote(self: &Rc<Self>, id: &str) {
        let mut kh = KnownHosts::load_internet().unwrap_or_default();
        let before = kh.hosts.len();
        kh.hosts.retain(|h| h.fingerprint != id);
        if kh.hosts.len() != before
            && let Err(e) = kh.save()
        {
            self.toast(&format!("Couldn't forget that PC: {e:#}"));
        }
        self.refresh_remote();
    }

    fn forget(self: &Rc<Self>, name: &str) {
        let mut kh = KnownHosts::load().unwrap_or_default();
        if kh.remove(name)
            && let Err(e) = kh.save()
        {
            self.toast(&format!("Couldn't forget {name}: {e:#}"));
        }
        self.refresh();
    }

    fn connect(self: &Rc<Self>, host: KnownHost) {
        if let Some(v) = self.viewer.take() {
            v.close();
        }
        let name = ClientConfig::load().map(|c| c.device_name()).unwrap_or_else(|_| ac_remote::hostname());
        let tab: Rc<dyn ViewerHost> = self.clone();
        match Viewer::open(host, name, &self.window, self.media.clone(), self.offload, tab) {
            Ok(v) => {
                self.viewer_slot.append(&v.root);
                *self.viewer.borrow_mut() = Some(v);
                self.root.set_visible_child_name("viewer");
            }
            Err(e) => self.toast(&format!("Couldn't connect: {e:#}")),
        }
    }

    /// Opens the paired PC with this fingerprint (`test-bed --connect`, which
    /// the other PC's share service runs when it switches sides).
    pub fn connect_fingerprint(self: &Rc<Self>, fingerprint: &str) {
        let host = KnownHosts::load().ok().and_then(|k| k.hosts.into_iter().find(|h| h.fingerprint == fingerprint));
        match host {
            Some(h) => {
                if self.viewer.borrow().as_ref().is_some_and(|v| v.fingerprint() == fingerprint) {
                    return;
                }
                self.connect(h)
            }
            None => self.toast("Couldn't switch sides: that PC isn't paired with this one"),
        }
    }

    /// While a remote PC has the keyboard, Test Bed's own shortcuts must not
    /// take its keys.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn grabbing(&self) -> bool {
        self.grabbing.get()
    }

    fn suspend_accels(&self, on: bool) {
        let Some(app) = self.window.application() else { return };
        if on {
            let mut saved = self.saved_accels.borrow_mut();
            for action in app.list_action_descriptions() {
                let accels = app.accels_for_action(&action);
                if !accels.is_empty() {
                    app.set_accels_for_action(&action, &[]);
                    saved.push((action.to_string(), accels.into_iter().collect()));
                }
            }
        } else {
            for (action, accels) in self.saved_accels.take() {
                let accels: Vec<&str> = accels.iter().map(|a| a.as_str()).collect();
                app.set_accels_for_action(&action, &accels);
            }
        }
    }
}

impl ViewerHost for RemoteTab {
    fn viewer_closed(&self) {
        self.root.set_visible_child_name("list");
        if let Some(v) = self.viewer.take() {
            tracing::info!("disconnected from {}", v.host_name());
        }
        while let Some(c) = self.viewer_slot.first_child() {
            self.viewer_slot.remove(&c);
        }
    }

    fn viewer_fullscreen(&self, on: bool) {
        self.toolbar.set_reveal_top_bars(!on);
        self.toolbar.set_reveal_bottom_bars(!on);
    }

    fn viewer_grabbing(&self, on: bool) {
        if self.grabbing.replace(on) != on {
            self.suspend_accels(on);
        }
    }

    fn viewer_message(&self, text: &str) {
        self.toast(text);
    }
}

/// "use1-1.relay.n0.iroh.link" from a relay URL, for the PC tab.
fn relay_host(url: Option<&str>) -> String {
    url.and_then(|u| u.split("://").nth(1))
        .map(|rest| rest.split(['/', ':']).next().unwrap_or(rest).trim_end_matches('.').to_string())
        .unwrap_or_else(|| "a relay".into())
}
