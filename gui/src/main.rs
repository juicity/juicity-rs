// On Windows, hide the console window for the release build of the GUI binary.
// The proxy child processes already suppress their own console via
// `CREATE_NO_WINDOW` (see `core.rs`); this hides the one the GUI itself spawns.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod about_dialog;
mod app;
mod config;
mod core;
mod i18n;
mod icon;
mod link;
mod log_dialog;
mod logging;
mod pac;
mod pac_dialog;
mod save_prompt;
mod startup_dialog;
mod state;
mod system_proxy;
mod system_theme;
mod traffic;
mod tray;
mod util;
mod widgets;

// Load translation files from `locales/` at compile time.
rust_i18n::i18n!("locales", fallback = "en");

fn main() -> anyhow::Result<()> {
    let log_level = std::env::args()
        .position(|arg| arg == "--log-level")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "info".to_string());

    // Console output plus the in-memory buffer the log window reads.
    logging::init(&log_level);

    // Activate the system locale before any UI string is read.
    i18n::init();

    app::run()
}
