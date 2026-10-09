// On Windows, hide the console window for the release build of the GUI binary.
// The proxy child processes already suppress their own console via
// `CREATE_NO_WINDOW` (see `core.rs`); this hides the one the GUI itself spawns.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(any(
    all(feature = "ui-gpui", feature = "ui-slint"),
    not(any(feature = "ui-gpui", feature = "ui-slint"))
))]
compile_error!("Enable exactly one of ui-gpui and ui-slint");

#[cfg(feature = "ui-gpui")]
mod about_dialog;
#[cfg(feature = "ui-gpui")]
mod app;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod config;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod core;
mod desktop;
mod i18n;
#[cfg(feature = "ui-gpui")]
mod icon;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod link;
#[cfg(feature = "ui-gpui")]
mod log_dialog;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod logging;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod pac;
#[cfg(feature = "ui-gpui")]
mod pac_dialog;
#[cfg(feature = "ui-gpui")]
mod save_prompt;
#[cfg(feature = "ui-gpui")]
mod startup_dialog;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod state;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod system_proxy;
#[cfg(feature = "ui-gpui")]
mod system_theme;
#[cfg(feature = "ui-gpui")]
mod traffic;
#[cfg(feature = "ui-gpui")]
mod tray;
#[cfg(feature = "ui-slint")]
mod ui;
#[cfg_attr(feature = "ui-slint", allow(dead_code))]
mod util;
mod validate;
mod version;
#[cfg(feature = "ui-gpui")]
mod widgets;

// Load translation files from `locales/` at compile time. The Slint UI does
// not use them, but `config.rs` labels still call `t!` until M5.
rust_i18n::i18n!("locales", fallback = "en");

#[cfg(any(feature = "ui-gpui", feature = "ui-slint"))]
fn main() -> anyhow::Result<()> {
    let log_level = std::env::args()
        .position(|arg| arg == "--log-level")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "info".to_string());

    // Console output plus the in-memory buffer the log window reads.
    logging::init(&log_level);

    // Activate the system locale before any UI string is read.
    #[cfg(feature = "ui-gpui")]
    i18n::init();

    #[cfg(feature = "ui-gpui")]
    return app::run();
    #[cfg(feature = "ui-slint")]
    return ui::run();
}

#[cfg(not(any(feature = "ui-gpui", feature = "ui-slint")))]
fn main() {}
