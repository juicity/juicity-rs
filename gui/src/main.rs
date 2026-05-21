mod app;
mod config;
mod core;
mod i18n;
mod icon;
mod link;
mod pac;
mod system_proxy;
mod tray;

// Load translation files from `locales/` at compile time.
rust_i18n::i18n!("locales", fallback = "en");

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Activate the system locale before any UI string is read.
    i18n::init();

    app::run()
}
