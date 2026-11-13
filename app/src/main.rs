// Windows builds the PC tab only, in its own window, without a console.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(unix)]
mod broadcast;
#[cfg(unix)]
mod dnd;
#[cfg(unix)]
mod grid_layout;
#[cfg(unix)]
mod mirror;
#[cfg(unix)]
mod pairing;
#[cfg(windows)]
mod pc_window;
#[cfg(unix)]
mod preferences;
#[cfg(unix)]
mod qr;
mod remote;
mod remote_viewer;
#[cfg(unix)]
mod slot_card;
#[cfg(unix)]
mod testbar;
mod texture;
#[cfg(unix)]
mod window;

#[cfg(unix)]
use std::cell::RefCell;
#[cfg(unix)]
use std::rc::Rc;
#[cfg(unix)]
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tracing_subscriber::EnvFilter;

const APP_ID: &str = "org.testbed.TestBed";

/// Logging, the command line and the application. `--connect <fingerprint>`
/// opens that paired PC in the PC tab; a Test Bed already running does it
/// (then `Err` holds the exit code).
fn start() -> Result<(adw::Application, Vec<String>, Option<String>), glib::ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,zbus=warn".into()))
        .init();

    let mut args: Vec<String> = std::env::args().collect();
    let connect = args.iter().position(|a| a == "--connect").and_then(|i| {
        let fp = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        fp
    });
    // Before anything else starts: a Test Bed already running takes it.
    let app = adw::Application::builder().application_id(APP_ID).build();
    if let Some(fp) = &connect
        && app.register(None::<&gtk::gio::Cancellable>).is_ok()
        && app.is_remote()
    {
        app.activate_action("remote-connect", Some(&fp.to_variant()));
        return Err(app.run_with_args(&args));
    }
    Ok((app, args, connect))
}

#[cfg(windows)]
fn main() -> glib::ExitCode {
    let (app, args, connect) = match start() {
        Ok(started) => started,
        Err(code) => return code,
    };
    // No VA-API on Windows: FFmpeg decodes in software.
    let media = std::sync::Arc::new(ac_media::MediaContext::new("software", "auto"));
    let connect = std::cell::RefCell::new(connect);
    app.connect_activate(move |app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        pc_window::build(app, media.clone());
        if let Some(fp) = connect.borrow_mut().take() {
            app.activate_action("remote-connect", Some(&fp.to_variant()));
        }
    });
    app.run_with_args(&args)
}

#[cfg(unix)]
fn main() -> glib::ExitCode {
    let (app, args, connect) = match start() {
        Ok(started) => started,
        Err(code) => return code,
    };

    let paths = ac_config::Paths::xdg();
    let (config, config_error) = match paths.as_ref().map(|p| ac_config::load_config(&p.config)) {
        Some(Ok(cfg)) => (cfg, None),
        Some(Err(e)) => {
            tracing::error!("{e}");
            (ac_config::Config::default(), Some(format!("Using default settings: {e}")))
        }
        None => (ac_config::Config::default(), None),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("ac-tokio")
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime");
    let slots = config.general.max_slots.clamp(1, ac_core::MAX_SLOTS as u8);
    let state_path = paths.as_ref().map(|p| p.state.clone());
    let config_path = paths.as_ref().map(|p| p.config.clone());
    let mut state = match state_path.as_deref().map(ac_config::load_state) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            tracing::warn!("{e}; using the default window state");
            ac_config::AppState::default()
        }
        None => ac_config::AppState::default(),
    };
    state.normalize(slots);
    let manager = {
        let _guard = runtime.enter();
        let mut opts = ac_device::ManagerOptions::new(config.clone(), paths);
        opts.control_socket = Some(ac_device::control::socket_path());
        opts.display_host = Some(std::sync::Arc::new(ac_device::RealHost));
        ac_device::spawn(opts)
    };

    let manager = Rc::new(RefCell::new(Some(manager)));
    let connect = RefCell::new(connect);
    app.connect_activate(move |app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        if let Some(m) = manager.borrow_mut().take() {
            let restore = window::Restore { state: state.clone(), path: state_path.clone(), config_path: config_path.clone() };
            window::build(app, m, &config, slots, config_error.clone(), restore);
            if let Some(fp) = connect.borrow_mut().take() {
                app.activate_action("remote-connect", Some(&fp.to_variant()));
            }
        }
    });
    let code = app.run_with_args(&args);
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}
