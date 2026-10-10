// On Windows, hide the console window for the release build of the GUI binary.
// The proxy child processes already suppress their own console via
// `CREATE_NO_WINDOW` (see `core.rs`); this hides the one the GUI itself spawns.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

// Modules marked `allow(dead_code)` keep helpers that only the removed
// frontend called; they are pruned in a separate pass.
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod core;
mod desktop;
mod i18n;
// Not called by the Slint frontend yet; kept for installing the theme icons.
#[allow(dead_code)]
mod icon;
mod link;
#[allow(dead_code)] // Used by the Slint logs page (next milestone).
mod logging;
mod pac;
#[allow(dead_code)]
mod state;
mod system_proxy;
#[allow(dead_code)] // Used by the Slint logs page (next milestone).
mod traffic;
mod ui;
mod util;
mod validate;
#[allow(dead_code)]
mod version;

fn main() -> anyhow::Result<()> {
    let log_level = std::env::args()
        .position(|arg| arg == "--log-level")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "info".to_string());

    // Console output plus the in-memory buffer the log window reads.
    logging::init(&log_level);

    ui::run()
}
