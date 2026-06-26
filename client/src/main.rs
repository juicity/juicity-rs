pub mod client;
pub mod forwarder;
pub mod local;

// Use Jemalloc for glibc/macOS; fall back to mimalloc for musl targets where
// jemalloc has known compatibility issues with musl's TLS and libc internals.
#[cfg(all(not(target_env = "musl"), not(target_os = "windows")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use juicity_common::config::Config;
use juicity_common::link;
use juicity_common::BuildInfo;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "juicity-client",
    about = "A QUIC-based proxy client",
    disable_version_flag = true
)]
struct Cli {
    /// Show version information
    #[arg(short = 'v', long = "version", help = "Print version information")]
    version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the proxy client
    Run {
        /// Config file path
        #[arg(short = 'c', long = "config")]
        config: String,

        /// Log level
        #[arg(long = "log-level", default_value = "info")]
        log_level: String,

        /// Set fwmark on the client socket (Linux only)
        #[arg(long)]
        fwmark: Option<u32>,
    },

    /// Export share link, QR code, or JSON config
    Export {
        /// Config file path
        #[arg(short = 'c', long = "config")]
        config: String,

        /// Print share link to stdout
        #[arg(long = "link")]
        link: bool,

        /// Print QR code to terminal
        #[arg(long = "qrcode")]
        qrcode: bool,

        /// Save QR code as PNG file
        #[arg(long = "qrcode-png")]
        qrcode_png: Option<String>,

        /// Export client config as JSON (fields kept as-is)
        #[arg(long = "json")]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default rustls CryptoProvider (aws-lc-rs)
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default rustls CryptoProvider");

    let cli = Cli::parse();

    // Handle -v/--version before any subcommand logic
    if cli.version {
        println!("{}", BuildInfo::version_string());
        return Ok(());
    }

    let Some(command) = cli.command else {
        // No subcommand and no --version flag; show help
        let mut cmd = <Cli as clap::CommandFactory>::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Commands::Run {
            config,
            log_level,
            fwmark,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(&log_level)),
                )
                .init();

            let mut config = Config::from_file(&config)?;
            if let Some(fwmark) = fwmark {
                config.fwmark = Some(fwmark);
            }
            config.validate_for_client()?;

            tracing::info!("Juicity client starting...");

            let client = client::JuicityClient::new(&config).await?;

            // Start forwarder if configured
            if !config.forward.is_empty() {
                let forwarder = forwarder::Forwarder::new(&config.forward, client.clone())?;
                tokio::spawn(async move {
                    if let Err(e) = forwarder.start().await {
                        tracing::error!("Forwarder error: {:?}", e);
                    }
                });
            }

            // Start local SOCKS5/HTTP proxy server if listen is configured
            if !config.listen.is_empty() {
                let local_server = local::LocalServer::new(config.listen.clone(), client);
                local_server.serve().await?;
            } else {
                // If only forward mode, keep the process alive
                tracing::info!("Running in forward-only mode");
                std::future::pending::<()>().await;
            }
        }

        Commands::Export {
            config,
            link: do_link,
            qrcode,
            qrcode_png,
            json,
        } => {
            let config = Config::from_file(&config)?;

            if do_link || qrcode || qrcode_png.is_some() {
                let share_link = link::generate_share_link(&config, None, None, None)
                    .map_err(|e| anyhow::anyhow!("Failed to generate share link: {}", e))?;

                if do_link {
                    println!("{}", share_link);
                }
                if qrcode {
                    link::print_qrcode(&share_link)?;
                }
                if let Some(path) = qrcode_png {
                    link::save_qrcode_png(&share_link, &path)?;
                }
            }

            if json {
                println!("{}", config.to_client_json()?);
            }
        }
    }

    Ok(())
}
