//! Monitor mode (spec2 §5): a phone shows a virtual Hyprland output.
//!
//! [`hypr`] manages the output, [`capture`] reads its pixels with
//! wlr-screencopy, [`encode`] makes H.264, [`session`] sends it to the phone,
//! [`touch`] and [`pointer`] turn the phone's touches into pointer input, and
//! [`keyboard`] its on-screen keyboard into typing. A phone can also mirror one
//! of the PC's own screens instead of a virtual one.

pub mod capture;
pub mod encode;
pub mod hypr;
pub mod keyboard;
pub mod pointer;
pub mod protocol;
pub mod session;
pub mod touch;

/// Names of the outputs this app creates, so leftovers can be recognised.
pub const OUTPUT_PREFIX: &str = "TBM-";

pub fn output_name(slot: u8) -> String {
    format!("{OUTPUT_PREFIX}{slot}")
}
