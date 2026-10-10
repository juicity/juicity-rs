// On Windows, hide the console window for the release build of the GUI binary.
// The proxy child processes already suppress their own console via
// `CREATE_NO_WINDOW` (see `core.rs`); this hides the one the GUI itself spawns.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod config;
mod core;
mod desktop;
mod i18n;
mod link;
mod logging;
mod pac;
mod state;
mod system_proxy;
mod traffic;
mod ui;
mod util;
mod validate;
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
