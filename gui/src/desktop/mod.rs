//! Desktop integration: autostart, single instance and the tray.

pub mod autostart;
#[cfg(target_os = "linux")]
pub mod integration;
#[cfg(target_os = "linux")]
pub mod session_bus;
pub mod single_instance;
pub mod tray;
