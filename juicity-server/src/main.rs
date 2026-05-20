use clap::{Parser, Subcommand};
use juicity_common::config::Config;
use juicity_common::link;
use juicity_common::BuildInfo;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "juicity-server",
    about = "A QUIC-based proxy server",
    disable_version_flag = true,
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
    /// Run the proxy server
    Run {
        /// Config file path
        #[arg(short = 'c', long = "config")]
        config: String,

        /// Log level
        #[arg(long = "log-level", default_value = "info")]
        log_level: String,
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

        /// Export server config as JSON
        #[arg(long = "json-server")]
        json_server: bool,

        /// Export client config derived from this server config as JSON
        #[arg(long = "json-client")]
        json_client: bool,

        /// SOCKS inbound listen port written into the exported client config
        #[arg(long = "socks-port", default_value = "1080")]
        socks_port: u16,
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
        Commands::Run { config, log_level } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(&log_level)),
                )
                .init();

            let config = Config::from_file(&config)?;
            config.validate_for_server()?;

            tracing::info!("Juicity server starting...");

            let srv = juicity_server::server::JuicityServer::new(&config).await?;
            srv.serve(&config.listen).await?;
        }

        Commands::Export {
            config,
            link: do_link,
            qrcode,
            qrcode_png,
            json_server,
            json_client,
            socks_port,
        } => {
            let config = Config::from_file(&config)?;

            if do_link || qrcode || qrcode_png.is_some() {
                let share_link = link::generate_share_link(&config)
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

            if json_server {
                println!("{}", config.to_server_json()?);
            }

            if json_client {
                println!("{}", config.to_client_json_from_server(socks_port)?);
            }
        }
    }

    Ok(())
}
