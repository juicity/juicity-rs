//! Desktop integration: autostart, single instance and the tray.

pub mod autostart;
#[cfg(target_os = "linux")]
pub mod integration;
pub mod single_instance;
pub mod tray;
