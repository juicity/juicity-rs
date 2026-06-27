mod app;
mod config;
mod core;
mod i18n;
mod icon;
mod link;
mod pac;
mod pac_dialog;
mod startup_dialog;
mod system_proxy;
mod tray;
mod util;

// Load translation files from `locales/` at compile time.
rust_i18n::i18n!("locales", fallback = "en");

fn main() -> anyhow::Result<()> {
    let log_level = std::env::args()
        .position(|arg| arg == "--log-level")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "info".to_string());

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log_level)),
        )
        .init();

    // Activate the system locale before any UI string is read.
    i18n::init();

    app::run()
}
