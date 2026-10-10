//! In-process proxy core manager.
//!
//! The GUI embeds both supported protocols directly instead of spawning
//! external helper binaries:
//!
//! * Juicity → the `juicity-client` crate (QUIC client plus the local
//!   SOCKS5/HTTP server), i.e. the same code that powers the CLI binary.
//! * Shadowsocks → `shadowsocks-service`, the official shadowsocks-rust
//!   library, which provides the SOCKS5 and HTTP local servers used by
//!   `sslocal`.
//!
//! Both services run on a dedicated Tokio runtime owned by this module, so the
//! GPUI event loop never blocks on proxy work.

use crate::config::{AppConfig, ProxyProfile, ProxyProtocol};
use anyhow::Context;
use juicity_client::client::JuicityClient;
use juicity_client::local::LocalServer;
use juicity_common::config::Config as JuicityConfig;
use shadowsocks_service::config::{
    Config as ShadowsocksConfig, ConfigType as ShadowsocksConfigType, LocalFlowStatAddress,
};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

/// How many times binding a listener is retried when the OS still holds the
/// previous core's socket while it shuts down.
const BIND_RETRY_ATTEMPTS: u32 = 5;
/// Delay between two bind retries.
const BIND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// A proxy core running in-process.
struct RunningCore {
    protocol: ProxyProtocol,
    /// Display name of the profile this core was started for.
    name: String,
    /// Task running the local proxy service.
    task: JoinHandle<()>,
    /// Keeps the QUIC endpoint (and its pooled connections) alive for the
    /// lifetime of a Juicity core; dropping it closes every connection.
    client: Option<JuicityClient>,
    /// Shadowsocks byte counters fed by its flow-statistics reporter.
    flow_stat: Option<Arc<FlowCounters>>,
    /// Last reported traffic, returned when a fresh reading is unavailable.
    last_traffic: (u64, u64),
}

/// Everything needed to supervise a freshly started core.
struct StartedCore {
    task: JoinHandle<()>,
    client: Option<JuicityClient>,
    flow_stat: Option<Arc<FlowCounters>>,
}

/// Cumulative byte counters pushed by shadowsocks-rust's flow-stat reporter.
#[derive(Default)]
struct FlowCounters {
    tx: AtomicU64,
    rx: AtomicU64,
    stop: AtomicBool,
}

impl FlowCounters {
    fn set(&self, tx: u64, rx: u64) {
        self.tx.store(tx, Ordering::Relaxed);
        self.rx.store(rx, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, u64) {
        (self.tx.load(Ordering::Relaxed), self.rx.load(Ordering::Relaxed))
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct CoreManager {
    /// Dedicated Tokio runtime hosting the cores, created on first use.
    runtime: Option<Runtime>,
    running: Option<RunningCore>,
}

impl CoreManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Name of the profile the running core was started with.
    pub fn current_name(&self) -> Option<&str> {
        self.running.as_ref().map(|v| v.name.as_str())
    }

    pub fn current_protocol(&self) -> Option<ProxyProtocol> {
        self.running.as_ref().map(|v| v.protocol)
    }

    /// Cumulative `(transmitted, received)` bytes for the running core, or
    /// `None` when no core is running.
    ///
    /// Juicity reads the QUIC connection statistics (UDP bytes); Shadowsocks
    /// reads the counters its local server pushes over the flow-statistics
    /// socket.  The last known value is returned when a fresh reading is
    /// momentarily unavailable.
    pub fn traffic(&mut self) -> Option<(u64, u64)> {
        let running = self.running.as_mut()?;
        let reading = match &running.client {
            Some(client) => client.traffic(),
            None => running.flow_stat.as_ref().map(|counters| counters.get()),
        };
        if let Some(reading) = reading {
            running.last_traffic = reading;
        }
        Some(running.last_traffic)
    }

    /// Lazily create the runtime that hosts the embedded cores.
    fn runtime(&mut self) -> anyhow::Result<&Runtime> {
        if self.runtime.is_none() {
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_workers)
                .max_blocking_threads(64)
                .enable_all()
                .thread_name("juicity-gui-core")
                .build()
                .context("failed to create the proxy runtime")?;
            self.runtime = Some(runtime);
        }
        Ok(self.runtime.as_ref().expect("the runtime was just created"))
    }

    pub fn start_profile(
        &mut self,
        config: &AppConfig,
        profile: &ProxyProfile,
    ) -> anyhow::Result<()> {
        // Release the previous core's listening sockets before rebinding them.
        self.stop_and_wait();

        tracing::info!(
            "starting in-process {:?} core for profile {}",
            profile.protocol,
            profile.name
        );

        let started = {
            let runtime = self.runtime()?;
            match profile.protocol {
                ProxyProtocol::Juicity => start_juicity(runtime, config, profile),
                ProxyProtocol::Shadowsocks => start_shadowsocks(runtime, config, profile),
            }
            .with_context(|| format!("failed to start {:?} core", profile.protocol))?
        };

        self.running = Some(RunningCore {
            protocol: profile.protocol,
            name: profile.display_name(),
            task: started.task,
            client: started.client,
            flow_stat: started.flow_stat,
            last_traffic: (0, 0),
        });
        Ok(())
    }

    /// Stop the running core.  Blocks until the service has released its
    /// listening sockets; the operation is short enough to stay on the UI
    /// thread.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        self.stop_and_wait();
        Ok(())
    }

    /// Stop the core and wait for it to shut down (used before a restart and
    /// on application exit).
    pub fn stop_and_wait(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        tracing::info!("stopping in-process {:?} core", running.protocol);

        let RunningCore {
            task,
            client,
            flow_stat,
            ..
        } = running;
        if let Some(flow_stat) = &flow_stat {
            flow_stat.stop();
        }
        // Dropping a `JoinHandle` only detaches the task, so abort explicitly.
        task.abort();
        if let Some(runtime) = self.runtime.as_ref() {
            // Wait for the aborted task so the service is dropped here rather
            // than lingering; `retry_on_addr_in_use` covers the sockets that
            // the runtime releases a moment later.
            let _ = runtime.block_on(task);
        }
        drop(client);
        drop(flow_stat);
    }

    /// Returns `None` while the core is healthy, or a human-readable reason
    /// when it has stopped unexpectedly.
    pub fn poll(&mut self) -> anyhow::Result<Option<String>> {
        let finished = self
            .running
            .as_ref()
            .map(|v| v.task.is_finished())
            .unwrap_or(false);
        if !finished {
            return Ok(None);
        }

        let running = self.running.take().expect("checked `is_finished` above");
        let RunningCore {
            protocol,
            task,
            client,
            ..
        } = running;

        let reason = match self.runtime.as_ref() {
            Some(runtime) => match runtime.block_on(task) {
                Ok(()) => format!("{protocol:?} core exited"),
                Err(err) if err.is_cancelled() => format!("{protocol:?} core exited"),
                Err(err) => format!("{protocol:?} core failed: {err}"),
            },
            None => format!("{protocol:?} core exited"),
        };
        drop(client);
        Ok(Some(reason))
    }
}

/// Start the embedded Juicity core (QUIC client + local SOCKS5/HTTP server).
fn start_juicity(
    runtime: &Runtime,
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<StartedCore> {
    juicity_client::install_default_crypto_provider();

    let juicity_config = build_juicity_config(config, profile)?;
    let listen = juicity_config.listen.clone();

    // Bind synchronously so that configuration errors, TLS setup failures and
    // "address already in use" are reported to the caller instead of being
    // buried in a detached task.
    let (client, listener) = runtime.block_on(async {
        let client = JuicityClient::new(&juicity_config).await?;
        let listener = retry_on_addr_in_use(BIND_RETRY_ATTEMPTS, BIND_RETRY_DELAY, || {
            TcpListener::bind(&listen)
        })
        .await
        .with_context(|| format!("failed to listen on {listen}"))?;
        Ok::<_, anyhow::Error>((client, listener))
    })?;

    let local_server = LocalServer::new(listen, client.clone());
    let task = runtime.handle().spawn(async move {
        if let Err(err) = local_server.serve_with_listener(listener).await {
            tracing::error!("juicity local server stopped: {err:#}");
        }
    });

    Ok(StartedCore {
        task,
        client: Some(client),
        flow_stat: None,
    })
}

/// Start the embedded Shadowsocks core using shadowsocks-rust.
fn start_shadowsocks(
    runtime: &Runtime,
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<StartedCore> {
    let mut shadowsocks_config = build_shadowsocks_config(config, profile)?;

    // Point the local server's flow-statistics reporter at a listener of ours:
    // shadowsocks-rust exposes no counter API, but it pushes the totals over a
    // socket when asked to, which is how we obtain its traffic.
    let (stat_addr, counters) = start_flow_stat_reader()?;
    shadowsocks_config.local_stat_addr = Some(LocalFlowStatAddress::TcpStreamAddr(stat_addr));

    // `Server::new` binds every listener, so failures surface here.  Stopping
    // the previous core aborts its tasks, which release their sockets
    // asynchronously, hence the bind retry.
    let server = runtime
        .block_on(retry_on_addr_in_use(
            BIND_RETRY_ATTEMPTS,
            BIND_RETRY_DELAY,
            || shadowsocks_service::local::Server::new(shadowsocks_config.clone()),
        ))
        .context("failed to start the shadowsocks local server");

    let server = match server {
        Ok(server) => server,
        Err(err) => {
            // Stop the reporter thread we already started.
            counters.stop();
            return Err(err);
        }
    };

    let task = runtime.handle().spawn(async move {
        if let Err(err) = server.run().await {
            tracing::error!("shadowsocks local server stopped: {err}");
        }
    });

    Ok(StartedCore {
        task,
        client: None,
        flow_stat: Some(counters),
    })
}

/// Bind a loopback listener that shadowsocks-rust reports its byte counters to.
///
/// The reporter connects every 500 ms and writes two native-endian `u64`
/// values — transmitted then received bytes — so a background thread reads them
/// and stores the latest totals in [`FlowCounters`].
fn start_flow_stat_reader() -> anyhow::Result<(SocketAddr, Arc<FlowCounters>)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("failed to bind the flow-statistics listener")?;
    let addr = listener.local_addr()?;
    listener
        .set_nonblocking(true)
        .context("failed to configure the flow-statistics listener")?;

    let counters = Arc::new(FlowCounters::default());
    let thread_counters = Arc::clone(&counters);
    std::thread::Builder::new()
        .name("ss-flow-stat".into())
        .spawn(move || {
            let mut buf = [0u8; 16];
            while !thread_counters.stopped() {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if read_flow_report(&mut stream, &mut buf).is_ok() {
                            let tx = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
                            let rx = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
                            thread_counters.set(tx, rx);
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        })
        .context("failed to spawn the flow-statistics thread")?;

    Ok((addr, counters))
}

/// Read the two counters of one flow-statistics report.
fn read_flow_report(stream: &mut std::net::TcpStream, buf: &mut [u8; 16]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    stream.read_exact(buf)
}

/// Retry `op` while it fails with [`std::io::ErrorKind::AddrInUse`].
///
/// Stopping a core aborts its tasks, and the runtime may need a moment to drop
/// them and release their listening sockets; restarting on the same port can
/// therefore briefly hit `EADDRINUSE`.
async fn retry_on_addr_in_use<T, F, Fut>(
    attempts: u32,
    delay: std::time::Duration,
    mut op: F,
) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    tokio::time::sleep(delay).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.expect("the loop only exits after at least one attempt"))
}

/// Build the Juicity client configuration for `profile`.
fn build_juicity_config(
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<JuicityConfig> {
    // Legacy profiles may point at a complete juicity-client config file.
    if let Some(path) = &profile.config_path {
        if path.exists() {
            let loaded = JuicityConfig::from_file(&path.to_string_lossy())
                .with_context(|| format!("failed to read {}", path.display()))?;
            loaded.validate_for_client()?;
            return Ok(loaded);
        }
    }

    let sni = profile
        .sni
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&profile.server)
        .to_string();

    let juicity_config = JuicityConfig {
        server: crate::util::format_host_port(&profile.server, profile.server_port),
        uuid: profile.uuid.clone(),
        password: profile.password.clone(),
        sni,
        allow_insecure: profile.allow_insecure,
        listen: config.mixed_listen.clone(),
        log_level: "info".to_string(),
        ..Default::default()
    };
    juicity_config.validate_for_client()?;
    Ok(juicity_config)
}

/// Build the shadowsocks-rust local configuration for `profile`.
///
/// The configuration is expressed in shadowsocks-rust's own JSON schema and
/// parsed with the library's parser, which keeps the GUI compatible with every
/// method, plugin and option the upstream project supports.
fn build_shadowsocks_config(
    config: &AppConfig,
    profile: &ProxyProfile,
) -> anyhow::Result<ShadowsocksConfig> {
    // Legacy profiles may point at a complete sslocal config file.
    if let Some(path) = &profile.config_path {
        if path.exists() {
            return ShadowsocksConfig::load_from_file(path, ShadowsocksConfigType::Local)
                .with_context(|| format!("failed to read {}", path.display()));
        }
    }

    let (addr, port) = crate::util::split_host_port(&config.mixed_listen);

    // A shadowsocks-rust "socks" local also answers HTTP proxy requests on the
    // same listener when built with `local-http`, which is exactly the mixed
    // inbound the GUI exposes.  A separate `"protocol": "http"` local would
    // need its own port.
    let mut json = serde_json::json!({
        "server": profile.server,
        "server_port": profile.server_port,
        "password": profile.password,
        "method": profile.method,
        "locals": [
            {
                "local_address": addr,
                "local_port": port,
                "protocol": "socks"
            }
        ],
        "timeout": profile.timeout
    });

    if let Some(plugin) = profile.plugin.as_deref().filter(|p| !p.is_empty()) {
        json["plugin"] = serde_json::Value::String(plugin.to_string());
        if let Some(opts) = profile.plugin_opts.as_deref().filter(|o| !o.is_empty()) {
            json["plugin_opts"] = serde_json::Value::String(opts.to_string());
        }
        if let Some(args) = profile.plugin_args.as_deref().filter(|a| !a.is_empty()) {
            let args: Vec<&str> = args.split_whitespace().collect();
            json["plugin_args"] = serde_json::json!(args);
        }
    }

    let json = serde_json::to_string(&json)?;
    ShadowsocksConfig::load_from_json_str(&json, ShadowsocksConfigType::Local)
        .map_err(|err| anyhow::anyhow!("invalid shadowsocks configuration: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shadowsocks_service::config::ProtocolType;

    fn ss_profile() -> ProxyProfile {
        ProxyProfile {
            protocol: ProxyProtocol::Shadowsocks,
            server: "ss.example.com".to_string(),
            server_port: 8388,
            password: "secret".to_string(),
            method: "chacha20-ietf-poly1305".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn shadowsocks_config_exposes_a_single_mixed_local() {
        let config = build_shadowsocks_config(&AppConfig::default(), &ss_profile()).unwrap();

        assert_eq!(config.server.len(), 1);
        assert_eq!(
            config.local.len(),
            1,
            "one mixed inbound, not one per protocol"
        );
        assert_eq!(config.local[0].config.protocol, ProtocolType::Socks);
        assert_eq!(
            config.local[0].config.addr,
            Some("127.0.0.1:1080".parse().unwrap())
        );
    }

    #[test]
    fn shadowsocks_config_accepts_plugins() {
        let profile = ProxyProfile {
            plugin: Some("v2ray-plugin".to_string()),
            plugin_opts: Some("mode=quic".to_string()),
            plugin_args: Some("-v --flag".to_string()),
            ..ss_profile()
        };

        let config = build_shadowsocks_config(&AppConfig::default(), &profile).unwrap();
        let plugin = config.server[0].config.plugin().expect("plugin is set");
        assert_eq!(plugin.plugin, "v2ray-plugin");
        assert_eq!(plugin.plugin_opts.as_deref(), Some("mode=quic"));
        assert_eq!(plugin.plugin_args, vec!["-v", "--flag"]);
    }

    #[test]
    fn shadowsocks_config_accepts_2022_methods() {
        use base64::Engine;

        // AEAD-2022 ciphers take a base64 PSK instead of a plain password.
        let key = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let profile = ProxyProfile {
            method: "2022-blake3-aes-256-gcm".to_string(),
            password: key,
            ..ss_profile()
        };

        let config = build_shadowsocks_config(&AppConfig::default(), &profile).unwrap();
        assert_eq!(config.server.len(), 1);
    }

    #[test]
    fn shadowsocks_config_rejects_unknown_method() {
        let profile = ProxyProfile {
            method: "definitely-not-a-cipher".to_string(),
            ..ss_profile()
        };
        assert!(build_shadowsocks_config(&AppConfig::default(), &profile).is_err());
    }

    #[test]
    fn juicity_config_defaults_sni_to_server() {
        let profile = ProxyProfile {
            protocol: ProxyProtocol::Juicity,
            server: "juicity.example.com".to_string(),
            server_port: 443,
            uuid: "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
            password: "secret".to_string(),
            ..Default::default()
        };

        let config = build_juicity_config(&AppConfig::default(), &profile).unwrap();
        assert_eq!(config.server, "juicity.example.com:443");
        assert_eq!(config.sni, "juicity.example.com");
        assert_eq!(config.listen, "127.0.0.1:1080");
    }

    #[test]
    fn juicity_config_uses_explicit_sni_and_ipv6_server() {
        let profile = ProxyProfile {
            protocol: ProxyProtocol::Juicity,
            server: "::1".to_string(),
            server_port: 8443,
            uuid: "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
            password: "secret".to_string(),
            sni: Some("front.example.com".to_string()),
            allow_insecure: true,
            ..Default::default()
        };

        let config = build_juicity_config(&AppConfig::default(), &profile).unwrap();
        assert_eq!(config.server, "[::1]:8443");
        assert_eq!(config.sni, "front.example.com");
        assert!(config.allow_insecure);
    }

    /// Exercises the real shadowsocks-service backend: the mixed inbound
    /// answers SOCKS5 and HTTP on one port, restarting reuses that port (the
    /// previous listener is released asynchronously) and stopping frees it.
    #[test]
    fn shadowsocks_mixed_inbound_serves_socks5_and_http() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let app_config = AppConfig {
            // Use an uncommon port so the test does not clash with a running proxy.
            mixed_listen: "127.0.0.1:38471".to_string(),
            ..Default::default()
        };
        let profile = ss_profile();
        let mut manager = CoreManager::new();

        manager.start_profile(&app_config, &profile).unwrap();
        assert!(manager.is_running());
        assert_eq!(manager.current_protocol(), Some(ProxyProtocol::Shadowsocks));
        assert_eq!(manager.current_name(), Some("ss.example.com:8388"));

        // SOCKS5 greeting -> "no authentication required".
        let mut socks = TcpStream::connect("127.0.0.1:38471").unwrap();
        socks
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socks.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut greeting = [0u8; 2];
        socks.read_exact(&mut greeting).unwrap();
        assert_eq!(greeting, [0x05, 0x00]);

        // A proxy request without a target host is rejected by the HTTP handler
        // with an error response on the very same port.
        let mut http = TcpStream::connect("127.0.0.1:38471").unwrap();
        http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        http.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut response = Vec::new();
        let mut chunk = [0u8; 64];
        while response.len() < 16 {
            let n = http.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..n]);
        }
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with("HTTP/1."),
            "expected an HTTP response on the mixed port, got {response:?}"
        );

        // Restarting must succeed even though the previous listener was just
        // released asynchronously.
        manager.start_profile(&app_config, &profile).unwrap();
        assert!(manager.is_running());

        manager.stop_and_wait();
        assert!(!manager.is_running());
        assert!(manager.poll().unwrap().is_none());
    }
}
