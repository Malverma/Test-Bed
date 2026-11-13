//! The Windows window: the PC tab on its own. Phones need adb, VA-API and
//! Wayland, so the phone side stays Linux only.

use std::sync::Arc;

use ac_media::MediaContext;
use adw::prelude::*;
use gtk::{gdk, glib};

const CSS: &str = r#"
.remote-bar { padding: 6px 12px; border-bottom: 1px solid alpha(currentColor, 0.1); }
.remote-panel { background: alpha(@window_bg_color, 0.9); color: @window_fg_color; border-radius: 12px; padding: 12px 16px; }
.video-frame { border-radius: 16px; background: black; }
.qr { border-radius: 12px; }
"#;

pub fn build(app: &adw::Application, media: Arc<MediaContext>) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = adw::ApplicationWindow::builder().application(app).title("Test Bed").default_width(960).default_height(720).build();
    let toasts = adw::ToastOverlay::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let remote = crate::remote::RemoteTab::new(&window, &toasts, &toolbar, Some(media), false);
    toasts.set_child(Some(&remote.root));

    // `test-bed --connect <fingerprint>`: another PC switched sides and its
    // share service asks this Test Bed to connect back.
    let connect = gtk::gio::SimpleAction::new("remote-connect", Some(glib::VariantTy::STRING));
    {
        let window = window.clone();
        connect.connect_activate(move |_, fp| {
            let Some(fp) = fp.and_then(|v| v.str()) else { return };
            window.present();
            remote.connect_fingerprint(fp);
        });
    }
    app.add_action(&connect);
    window.present();
}
