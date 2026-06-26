// Use Jemalloc for glibc/macOS; fall back to mimalloc for musl targets where
// jemalloc has known compatibility issues with musl's TLS and libc internals.
#[cfg(all(not(target_env = "musl"), not(target_os = "windows")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use juicity_common::cert;
use juicity_common::config::Config;
use juicity_common::link;
use juicity_common::BuildInfo;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "juicity-server",
    about = "A QUIC-based proxy server",
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

        /// Network interface name to use for the share link host (e.g. eth0).
        /// If not specified, an interactive selection will be shown.
        #[arg(long = "interface")]
        interface: Option<String>,

        /// Domain to use as SNI in the share link.
        /// If not specified and the certificate is a wildcard or multi-domain,
        /// an interactive prompt will be shown.
        #[arg(long = "domain")]
        domain: Option<String>,

        /// Enable to include pinned_certchain_sha256 in the share link,
        /// computed from the server's certificate file.
        #[arg(long = "with-cert-sha256", default_value_t = false)]
        with_cert_sha256: bool,
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
            interface,
            domain,
            with_cert_sha256,
        } => {
            let config = Config::from_file(&config)?;

            // If no output flag is given, default to interactive share-link mode.
            let default_mode =
                !do_link && !qrcode && qrcode_png.is_none() && !json_server && !json_client;
            let do_link = do_link || default_mode;

            // Compute certificate SHA256 if requested
            let cert_sha256_override = if with_cert_sha256 {
                if config.certificate.is_empty() {
                    eprintln!(
                        "Warning: --with-cert-sha256 specified but no certificate path in config"
                    );
                    None
                } else {
                    match compute_cert_sha256(&config.certificate) {
                        Ok(hash) => Some(hash),
                        Err(e) => {
                            eprintln!("Warning: failed to compute certificate SHA256: {}", e);
                            None
                        }
                    }
                }
            } else {
                None
            };

            if do_link || qrcode || qrcode_png.is_some() {
                // Step 1: Resolve hosts (from --interface or interactive multi-selection)
                let hosts = resolve_export_hosts(interface.as_deref())?;

                // Step 2: Resolve SNI once (same for all hosts)
                let sni_override = resolve_export_sni(&config, domain.as_deref())?;

                // Step 3: Generate one share link per host
                for host in &hosts {
                    let share_link = link::generate_share_link(
                        &config,
                        Some(host.as_str()),
                        sni_override.as_deref(),
                        cert_sha256_override.as_deref(),
                    )
                    .map_err(|e| anyhow::anyhow!("Failed to generate share link: {}", e))?;

                    // Step 4: Output
                    if do_link {
                        println!("{}", share_link);
                    }
                    if qrcode {
                        link::print_qrcode(&share_link)?;
                    }
                    if let Some(ref base_path) = qrcode_png {
                        // When multiple hosts are selected, disambiguate file names.
                        let out_path = if hosts.len() > 1 {
                            let safe = host.replace(['[', ']', ':'], "_");
                            match base_path.rsplit_once('.') {
                                Some((stem, ext)) => format!("{}-{}.{}", stem, safe, ext),
                                None => format!("{}-{}", base_path, safe),
                            }
                        } else {
                            base_path.clone()
                        };
                        link::save_qrcode_png(&share_link, &out_path)?;
                    }
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

// ── Host Resolution ──

/// Resolve the list of hosts to use in share links.
///
/// Priority:
/// 1. If `--interface` is specified, collect all IPs of that interface.
/// 2. Otherwise, interactively list every non-loopback address and let the
///    user pick one or more (multi-select).
fn resolve_export_hosts(interface: Option<&str>) -> anyhow::Result<Vec<String>> {
    match interface {
        Some(iface) => get_interface_ips(iface),
        None => interactive_select_interfaces(),
    }
}

/// Collect all IP addresses of a specific network interface.
///
/// IPv4 addresses are returned as-is; IPv6 addresses are wrapped in `[…]`
/// so they can be embedded directly into a `host:port` URL.
fn get_interface_ips(interface_name: &str) -> anyhow::Result<Vec<String>> {
    let addrs = if_addrs::get_if_addrs()?;
    let mut found = false;
    let mut ips: Vec<String> = Vec::new();

    for addr in addrs.iter().filter(|a| a.name == interface_name) {
        found = true;
        match &addr.addr {
            if_addrs::IfAddr::V4(v4) => ips.push(v4.ip.to_string()),
            if_addrs::IfAddr::V6(v6) => ips.push(format!("[{}]", v6.ip)),
        }
    }

    if !found {
        anyhow::bail!("Interface '{}' not found", interface_name);
    }
    if ips.is_empty() {
        anyhow::bail!("Interface '{}' has no IP address", interface_name);
    }
    Ok(ips)
}

/// Interactively multi-select addresses from all non-loopback interfaces.
///
/// Lists every (interface, address) pair — both IPv4 and IPv6 — and lets
/// the user toggle any number of them.  At least one must be chosen.
fn interactive_select_interfaces() -> anyhow::Result<Vec<String>> {
    use dialoguer::MultiSelect;

    let addrs = if_addrs::get_if_addrs()?;
    // (display label, url-ready ip string)
    let mut candidates: Vec<(String, String)> = Vec::new();

    for addr in &addrs {
        if addr.is_loopback() {
            continue;
        }
        match &addr.addr {
            if_addrs::IfAddr::V4(v4) => {
                candidates.push((format!("{} ({})", addr.name, v4.ip), v4.ip.to_string()))
            }
            if_addrs::IfAddr::V6(v6) => candidates.push((
                format!("{} ([{}])", addr.name, v6.ip),
                format!("[{}]", v6.ip),
            )),
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("No network interfaces with IP addresses found");
    }

    let items: Vec<&str> = candidates.iter().map(|(label, _)| label.as_str()).collect();

    let selections = MultiSelect::new()
        .with_prompt("Select addresses for share link (space to toggle, enter to confirm)")
        .items(&items)
        .interact()?;

    if selections.is_empty() {
        anyhow::bail!("No address selected");
    }

    Ok(selections
        .iter()
        .map(|&i| candidates[i].1.clone())
        .collect())
}

// ── SNI Resolution ──

/// Resolve the SNI to use in the share link.
///
/// Priority:
/// 1. If `--domain` is specified, use it directly.
/// 2. Parse the TLS certificate and determine the best domain:
///    - Single non-wildcard domain → auto-use
///    - Wildcard-only → interactive prompt for specific domain
///    - Multiple domains → interactive selection
/// 3. If certificate parsing fails, fall back to `config.sni`.
fn resolve_export_sni(config: &Config, domain: Option<&str>) -> anyhow::Result<Option<String>> {
    // User explicitly specified a domain → use it directly
    if let Some(d) = domain {
        return Ok(Some(d.to_string()));
    }

    // No certificate path → fall back to config.sni
    if config.certificate.is_empty() {
        return Ok(if config.sni.is_empty() {
            None
        } else {
            Some(config.sni.clone())
        });
    }

    // Try to parse the certificate
    match cert::parse_cert_domains(&config.certificate) {
        Ok(domains) => {
            // Single non-wildcard domain → auto-use
            if let Some(preferred) = cert::pick_preferred_domain(&domains, None) {
                return Ok(Some(preferred));
            }

            // Wildcard-only → prompt user to enter specific domain
            if domains.is_wildcard && domains.sans.len() <= 1 {
                let wildcard_pattern = domains.sans.first().or(domains.cn.as_ref()).unwrap();
                eprintln!("Certificate is a wildcard ({})", wildcard_pattern);
                return Ok(Some(interactive_input_domain(wildcard_pattern)?));
            }

            // Multiple domains (including mixed wildcard + specific) → let user choose
            if !domains.sans.is_empty() {
                return Ok(Some(interactive_select_domain(&domains.sans)?));
            }

            // No SANs and CN is wildcard → shouldn't happen, but fallback
            if let Some(cn) = &domains.cn {
                return Ok(Some(cn.clone()));
            }

            Ok(None)
        }
        Err(_) => {
            // Certificate parsing failed → fall back to config.sni
            Ok(if config.sni.is_empty() {
                None
            } else {
                Some(config.sni.clone())
            })
        }
    }
}

/// Interactively prompt the user to enter a specific domain for a wildcard certificate.
fn interactive_input_domain(wildcard_pattern: &str) -> anyhow::Result<String> {
    use dialoguer::Input;

    let domain: String = Input::new()
        .with_prompt(format!(
            "Enter the specific domain for SNI (wildcard: {})",
            wildcard_pattern
        ))
        .interact_text()?;

    if domain.is_empty() {
        anyhow::bail!("Domain cannot be empty");
    }
    Ok(domain)
}

/// Interactively let the user select one domain from multiple certificate SANs.
fn interactive_select_domain(domains: &[String]) -> anyhow::Result<String> {
    use dialoguer::Select;

    let selection = Select::new()
        .with_prompt("Certificate contains multiple domains, select one for SNI")
        .items(domains)
        .default(0)
        .interact()?;

    Ok(domains[selection].clone())
}

// ── Certificate SHA256 Computation ──

/// Read a PEM certificate file and compute the SHA256 hash of its DER content.
///
/// Returns the hash as a 64-character lowercase hex string suitable for use
/// as `pinned_certchain_sha256` in a share link.
fn compute_cert_sha256(cert_path: &str) -> anyhow::Result<String> {
    use std::fs;
    use std::io::Read;

    let mut pem_data = Vec::new();
    fs::File::open(cert_path)?.read_to_end(&mut pem_data)?;

    let mut pem_reader = std::io::BufReader::new(pem_data.as_slice());
    let item = rustls_pemfile::read_one(&mut pem_reader)?
        .ok_or_else(|| anyhow::anyhow!("No PEM data found in {}", cert_path))?;

    let cert_der = match item {
        rustls_pemfile::Item::X509Certificate(der) => der,
        _ => anyhow::bail!("Expected X509 certificate in {}", cert_path),
    };

    let mut hasher = Sha256::new();
    hasher.update(&cert_der);
    let hash = hasher.finalize();

    Ok(hex::encode(hash))
}
