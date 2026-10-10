//! Desktop integration shared by both frontends.

pub mod autostart;
#[cfg(feature = "ui-slint")]
pub mod single_instance;
#[cfg(feature = "ui-slint")]
pub mod tray;
