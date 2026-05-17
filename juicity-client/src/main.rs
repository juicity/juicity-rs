pub mod client;
pub mod forwarder;
pub mod local;

use clap::Parser;
use juicity_common::config::Config;
use juicity_common::link;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "juicity-client", about = "A QUIC-based proxy client")]
struct Args {
    /// Config file path
    #[arg(short = 'c', long = "config")]
    config: String,

    /// Log level
    #[arg(long = "log-level", default_value = "info")]
    log_level: String,

    /// Generate a share link from config and print to terminal
    #[arg(long = "gen-link")]
    gen_link: bool,

    /// Generate a QR code from config and print to terminal
    #[arg(long = "gen-qrcode")]
    gen_qrcode: bool,

    /// Generate a QR code from config and save as PNG file
    #[arg(long = "gen-qrcode-png")]
    gen_qrcode_png: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default rustls CryptoProvider (aws-lc-rs)
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default rustls CryptoProvider");

    let args = Args::parse();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&args.log_level)),
        )
        .init();

    let config = Config::from_file(&args.config)?;

    // Handle share-link / QR code generation modes
    if args.gen_link || args.gen_qrcode || args.gen_qrcode_png.is_some() {
        let link = link::generate_share_link(&config)
            .map_err(|e| anyhow::anyhow!("Failed to generate share link: {}", e))?;

        if args.gen_link {
            println!("{}", link);
        }

        if args.gen_qrcode {
            link::print_qrcode(&link)?;
        }

        if let Some(path) = args.gen_qrcode_png {
            link::save_qrcode_png(&link, &path)?;
        }

        return Ok(());
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

    Ok(())
}
