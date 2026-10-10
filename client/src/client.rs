use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use juicity_common::consts;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, RecvStream, SendStream, VarInt};
use uuid::Uuid;

#[derive(Default)]
struct ReconnectState {
    last_failure: Option<(std::time::Instant, Arc<anyhow::Error>)>,
    attempts: u32,
}

fn parse_server(server: &str, sni: &str) -> anyhow::Result<(String, u16, String)> {
    let (host, port) = if let Ok(addr) = server.parse::<SocketAddr>() {
        (addr.ip().to_string(), addr.port())
    } else {
        let (host, port) =
            juicity_common::link::parse_host_port(server).map_err(anyhow::Error::msg)?;
        if host.is_empty() || host.contains(':') || server.starts_with('[') {
            anyhow::bail!("Invalid server address: {}", server);
        }
        (host, port)
    };
    let sni = if sni.is_empty() {
        host.clone()
    } else {
        sni.to_string()
    };
    Ok((host, port, sni))
}

fn reconnect_delay(attempts: u32) -> std::time::Duration {
    std::time::Duration::from_secs((1u64 << attempts.min(5)).min(30))
}

async fn try_addresses<T, F, Fut>(
    addresses: impl IntoIterator<Item = SocketAddr>,
    mut connect: F,
) -> anyhow::Result<T>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut error = None;
    for addr in addresses {
        match tokio::time::timeout(consts::DEFAULT_DIAL_TIMEOUT, connect(addr)).await {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(e)) => error = Some(e),
            Err(e) => {
                error = Some(anyhow::Error::new(e).context("QUIC connection attempt timed out"))
            }
        }
    }
    Err(error.unwrap_or_else(|| anyhow::anyhow!("No addresses found for server")))
}

async fn resolve_with_timeout<T>(
    lookup: impl Future<Output = std::io::Result<T>>,
    timeout: std::time::Duration,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, lookup)
        .await
        .context("DNS lookup timed out")?
        .context("DNS lookup failed")
}

fn udp_packet_len(data: &[u8]) -> anyhow::Result<[u8; 2]> {
    Ok(u16::try_from(data.len())
        .map_err(|_| anyhow::anyhow!("UDP payload exceeds 65535 bytes"))?
        .to_be_bytes())
}

/// A Juicity client multiplexing streams over one authenticated QUIC connection.
#[derive(Clone)]
pub struct JuicityClient {
    endpoint: Arc<Endpoint>,
    server_host: String,
    server_port: u16,
    uuid: Uuid,
    password: zeroize::Zeroizing<String>,
    sni: String,
    quic_config: Arc<ClientConfig>,
    connection: Arc<tokio::sync::RwLock<Option<Connection>>>,
    /// Keep the authentication stream open for underlay authentication messages.
    auth_uni_stream: Arc<tokio::sync::Mutex<Option<SendStream>>>,
    reconnect_lock: Arc<tokio::sync::Mutex<ReconnectState>>,
}

impl JuicityClient {
    /// Build a TLS client config based on the allow_insecure / pinned_certchain_sha256 settings.
    fn build_tls_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
        enable_early_data: bool,
    ) -> anyhow::Result<rustls::ClientConfig> {
        let mut tls_config: rustls::ClientConfig = if allow_insecure {
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipVerify::new(provider.clone())))
                .with_no_client_auth()
        } else if !pinned_hash.is_empty() {
            let hash_clone = pinned_hash.to_vec();
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinVerify::new(
                    provider.clone(),
                    hash_clone,
                )))
                .with_no_client_auth()
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder_with_provider(provider.clone().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        // Juicity spec requires ALPN to be h3.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        // Enable 0-RTT (Early Data) to reduce reconnection latency
        tls_config.enable_early_data = enable_early_data;

        Ok(tls_config)
    }

    /// Build a QUIC client config (TLS + transport settings).
    fn build_quic_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
        congestion_control: &str,
        initial_rtt: Option<u64>,
        keep_alive_interval: Option<u64>,
        enable_0rtt: bool,
    ) -> anyhow::Result<ClientConfig> {
        if allow_insecure {
            tracing::warn!("TLS certificate verification is DISABLED (allow_insecure=true). This is insecure and should only be used for testing.");
        }
        let tls_config =
            Self::build_tls_config(allow_insecure, pinned_hash, provider, enable_0rtt)?;

        let mut quic_config = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));

        let mut transport_config = quinn::TransportConfig::default();

        // Set initial_rtt if configured
        if let Some(initial_rtt_ms) = initial_rtt {
            transport_config.initial_rtt(std::time::Duration::from_millis(initial_rtt_ms));
        }

        // Keep-alive is disabled by default so QUIC idle timeout / connection
        // lifecycle management can release idle connections naturally.
        // Enable only when explicitly configured.
        if let Some(keep_alive_secs) = keep_alive_interval {
            transport_config
                .keep_alive_interval(Some(std::time::Duration::from_secs(keep_alive_secs)));
        }

        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config
            .max_concurrent_uni_streams(VarInt::from_u32(consts::MAX_OPEN_INCOMING_STREAMS as u32));
        // Set an explicit idle timeout for defense-in-depth.
        transport_config.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(consts::MAX_QUIC_IDLE_TIMEOUT)
                .map_err(|e| anyhow::anyhow!("invalid idle timeout: {:?}", e))?,
        ));
        transport_config
            .stream_receive_window(VarInt::from_u32(consts::QUIC_STREAM_RECEIVE_WINDOW));
        transport_config.receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW));
        transport_config.send_window(consts::QUIC_SEND_WINDOW);

        // Dynamically adjust window size based on initial_rtt
        if let Some(rtt_ms) = initial_rtt {
            if rtt_ms < 50 {
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW / 2,
                ));
                transport_config
                    .receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW / 2));
            } else if rtt_ms > 200 {
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW * 2,
                ));
                transport_config
                    .receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW * 2));
            }
        }

        match congestion_control.to_lowercase().as_str() {
            "cubic" => transport_config
                .congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
            "newreno" | "new_reno" => transport_config.congestion_controller_factory(Arc::new(
                quinn::congestion::NewRenoConfig::default(),
            )),
            _ => {
                let mut bbr_config = quinn::congestion::BbrConfig::default();
                bbr_config.initial_window(10 * consts::ETHERNET_MTU as u64);
                transport_config.congestion_controller_factory(Arc::new(bbr_config))
            }
        };
        quic_config.transport_config(Arc::new(transport_config));

        Ok(quic_config)
    }

    /// Create a client without connecting. Resolve the server on each reconnect.
    /// An empty SNI uses the server hostname, or the canonical IP for literals.
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let uuid = Uuid::parse_str(&config.uuid)?;
        let (server_host, server_port, sni) = parse_server(&config.server, &config.sni)?;

        let pinned_hash = if config.pinned_certchain_sha256.is_empty() {
            Vec::new()
        } else {
            use base64::Engine;
            let engine_url = base64::engine::general_purpose::URL_SAFE;
            if let Ok(hash) = engine_url.decode(&config.pinned_certchain_sha256) {
                hash
            } else {
                let engine_std = base64::engine::general_purpose::STANDARD;
                if let Ok(hash) = engine_std.decode(&config.pinned_certchain_sha256) {
                    hash
                } else {
                    hex::decode(&config.pinned_certchain_sha256)?
                }
            }
        };

        let bind_addr: SocketAddr = "[::]:0".parse()?;

        let endpoint = if let Some(fwmark) = config.fwmark {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use socket2::{Domain, Protocol, Socket, Type};

                let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                sock.set_only_v6(false)?;

                #[cfg(target_os = "linux")]
                sock.set_mark(fwmark)?;

                #[cfg(not(target_os = "linux"))]
                println!(
                    "Warning: fwmark is only supported on Linux, ignoring fwmark={}",
                    fwmark
                );

                sock.bind(&bind_addr.into())?;
                let std_socket: std::net::UdpSocket = sock.into();

                let runtime = quinn::default_runtime()
                    .ok_or_else(|| anyhow::anyhow!("No quinn runtime available"))?;
                let wrapped = runtime.wrap_udp_socket(std_socket)?;

                let endpoint = Endpoint::new_with_abstract_socket(
                    EndpointConfig::default(),
                    None,
                    wrapped,
                    runtime,
                )?;
                Ok(endpoint)
            })
            .await??
        } else {
            tokio::task::spawn_blocking(move || Endpoint::client(bind_addr)).await??
        };
        let endpoint = Arc::new(endpoint);

        // Build and cache the QUIC client config once.
        let allow_insecure = config.allow_insecure;
        let pinned_hash_for_config = pinned_hash.clone();
        let cc = config.congestion_control.clone();
        let initial_rtt = config.initial_rtt;
        let keep_alive_interval = config.keep_alive_interval;
        let enable_0rtt = config.enable_0rtt.unwrap_or(true);
        let quic_config = tokio::task::spawn_blocking(move || {
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            Self::build_quic_config(
                allow_insecure,
                &pinned_hash_for_config,
                &provider,
                &cc,
                initial_rtt,
                keep_alive_interval,
                enable_0rtt,
            )
        })
        .await??;
        let quic_config = Arc::new(quic_config);

        Ok(Self {
            endpoint,
            server_host,
            server_port,
            uuid,
            password: zeroize::Zeroizing::new(config.password.clone()),
            sni,
            quic_config,
            connection: Arc::new(tokio::sync::RwLock::new(None)),
            auth_uni_stream: Arc::new(tokio::sync::Mutex::new(None)),
            reconnect_lock: Arc::new(tokio::sync::Mutex::new(ReconnectState::default())),
        })
    }

    /// Cumulative `(transmitted, received)` bytes across the live QUIC
    /// connection.
    ///
    /// These are UDP-level counters, i.e. everything the tunnel carried over
    /// the wire including QUIC overhead.  Returns `None` while the connection
    /// is being rebuilt by a reconnect, which the caller can treat as "no
    /// reading right now".
    pub fn traffic(&self) -> Option<(u64, u64)> {
        let guard = self.connection.try_read().ok()?;
        let conn = guard.as_ref()?;
        let stats = conn.stats();
        Some((stats.udp_tx.bytes, stats.udp_rx.bytes))
    }

    /// Return the shared connection, reconnecting if necessary.
    /// Failures are shared with queued callers until the backoff window expires.
    pub async fn connect(&self) -> anyhow::Result<Connection> {
        self.connect_inner(true).await
    }

    /// Warm up the shared connection without recording a failure in backoff.
    pub async fn preconnect(&self) -> anyhow::Result<Connection> {
        self.connect_inner(false).await
    }

    async fn connect_inner(&self, record_failure: bool) -> anyhow::Result<Connection> {
        {
            let guard = self.connection.read().await;
            if let Some(conn) = guard.as_ref().filter(|conn| conn.close_reason().is_none()) {
                return Ok(conn.clone());
            }
        }

        let mut reconnect = self.reconnect_lock.lock().await;
        {
            let guard = self.connection.read().await;
            if let Some(conn) = guard.as_ref().filter(|conn| conn.close_reason().is_none()) {
                return Ok(conn.clone());
            }
        }

        if let Some((last, error)) = &reconnect.last_failure {
            if last.elapsed() < reconnect_delay(reconnect.attempts) {
                return Err(anyhow::anyhow!("{error:#}"));
            }
        }

        *self.connection.write().await = None;
        *self.auth_uni_stream.lock().await = None;

        tracing::info!(
            "Connecting to Juicity server at {}:{}",
            self.server_host,
            self.server_port
        );

        let connect_result = (async {
            let addresses = resolve_with_timeout(
                tokio::net::lookup_host((self.server_host.as_str(), self.server_port)),
                consts::DNS_QUERY_TIMEOUT,
            )
            .await?;
            try_addresses(addresses, |addr| self.connect_to_address(addr)).await
        })
        .await;

        let (quinn_conn, uni) = match connect_result {
            Ok(pair) => pair,
            Err(error) => {
                if record_failure {
                    reconnect.last_failure =
                        Some((std::time::Instant::now(), Arc::new(anyhow::anyhow!("{error:#}"))));
                    reconnect.attempts = reconnect.attempts.saturating_add(1);
                }
                return Err(error);
            }
        };

        tracing::info!("Authenticated as user {}", self.uuid);

        // Publish only after the auth stream is available to concurrent callers.
        *self.auth_uni_stream.lock().await = Some(uni);
        *self.connection.write().await = Some(quinn_conn.clone());
        *reconnect = ReconnectState::default();

        Ok(quinn_conn)
    }

    async fn connect_to_address(&self, addr: SocketAddr) -> anyhow::Result<(Connection, SendStream)> {
        let quinn_conn = self
            .endpoint
            .connect_with((*self.quic_config).clone(), addr, &self.sni)?
            .await
            .context("QUIC handshake failed")?;
        let mut uni = quinn_conn.open_uni().await?;

        let conn_for_token = quinn_conn.clone();
        let uuid_for_token = self.uuid;
        let password_for_token = (*self.password).clone();
        let token = tokio::task::spawn_blocking(move || {
            protocol::gen_token_via_connection(
                &conn_for_token,
                &uuid_for_token,
                &password_for_token,
            )
        })
        .await??;

        let mut auth_buf = [0u8; 50];
        auth_buf[0] = protocol::PROTOCOL_VERSION;
        auth_buf[1] = protocol::AUTHENTICATE_TYPE;
        auth_buf[2..18].copy_from_slice(self.uuid.as_bytes());
        auth_buf[18..50].copy_from_slice(&token);
        uni.write_all(&auth_buf).await?;
        Ok((quinn_conn, uni))
    }

    /// Send one underlay authentication message on the persistent auth uni stream.
    pub async fn send_underlay_auth(&self, auth: &protocol::UnderlayAuth) -> anyhow::Result<()> {
        self.connect().await?;

        let mut auth_guard = self.auth_uni_stream.lock().await;
        let stream = auth_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("auth uni stream not available"))?;

        if let Err(e) = protocol::write_underlay_auth_async(stream, auth).await {
            *auth_guard = None;
            return Err(e);
        }
        Ok(())
    }

    /// Open a TCP stream: sends proxy_header(TCP) once
    pub async fn open_tcp_stream(
        &self,
        addr: &str,
        port: u16,
    ) -> anyhow::Result<(SendStream, RecvStream)> {
        let conn = self.connect().await?;
        let (mut send, recv) = conn.open_bi().await?;

        // Build and send proxy header: [network=TCP(1)][addr_type][addr][port]
        let header = protocol::build_proxy_header(protocol::NETWORK_TCP, addr, port)?;
        send.write_all(&header).await?;

        Ok((send, recv))
    }

    /// Open a UDP stream with first datagram.
    /// Reject payloads above 65535 bytes before connecting or writing.
    ///
    /// Wire format (upstream-compatible):
    ///   stream header:   [network=3][trojanc_addr]
    ///   first datagram:  [trojanc_addr][len(2)][payload]
    pub async fn open_udp_stream(
        &self,
        addr: &str,
        port: u16,
        first_packet: &[u8],
    ) -> anyhow::Result<(SendStream, RecvStream)> {
        let pkt_len = udp_packet_len(first_packet)?;
        let conn = self.connect().await?;
        let (mut send, recv) = conn.open_bi().await?;

        // Batch stream header + first datagram into a single write:
        //   stream header:  [network=3][trojanc_addr]
        //   first datagram: [trojanc_addr][len(2)][payload]
        let stream_header = protocol::build_proxy_header(protocol::NETWORK_UDP, addr, port)?;
        let dgram_addr = protocol::build_trojanc_addr(addr, port)?;
        let mut buf =
            Vec::with_capacity(stream_header.len() + dgram_addr.len() + 2 + first_packet.len());
        buf.extend_from_slice(&stream_header);
        buf.extend_from_slice(&dgram_addr);
        buf.extend_from_slice(&pkt_len);
        buf.extend_from_slice(first_packet);
        send.write_all(&buf).await?;

        Ok((send, recv))
    }

    /// Send a subsequent UDP datagram on an existing stream.
    ///
    /// Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
    /// No leading network byte — each datagram carries only its own address.
    ///
    /// The `addr_buf` is a reusable scratch buffer to avoid per-packet heap
    /// allocation. It is cleared before each use.
    /// Send a subsequent UDP datagram on an existing stream.
    ///
    /// Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
    /// No leading network byte — each datagram carries only its own address.
    ///
    /// The `addr_buf` is a reusable scratch buffer to avoid per-packet heap
    /// allocation. All three wire segments are batched into a single
    /// `write_all` call to minimise QUIC stream lock acquisitions.
    /// Reject payloads above 65535 bytes without writing or changing `addr_buf`.
    pub async fn send_udp_datagram(
        send: &mut SendStream,
        addr: &str,
        port: u16,
        data: &[u8],
        addr_buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let len = udp_packet_len(data)?;
        let cached = protocol::CachedAddr::from_host_port(addr, port);
        addr_buf.clear();
        protocol::build_trojanc_addr_cached(addr_buf, &cached)?;
        // Batch address header + length + payload into a single write_all
        // to reduce QUIC stream lock acquisitions from 3 to 1.
        let total_len = addr_buf.len() + 2 + data.len();
        addr_buf.reserve(total_len - addr_buf.len());
        addr_buf.extend_from_slice(&len);
        addr_buf.extend_from_slice(data);
        send.write_all(addr_buf).await?;
        Ok(())
    }
}

// ── TLS certificate verifiers ──

#[derive(Debug)]
struct SkipVerify {
    provider: rustls::crypto::CryptoProvider,
}
impl SkipVerify {
    fn new(provider: rustls::crypto::CryptoProvider) -> Self {
        Self { provider }
    }
}
impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinVerify {
    provider: rustls::crypto::CryptoProvider,
    pinned_hash: Vec<u8>,
}
impl PinVerify {
    fn new(provider: rustls::crypto::CryptoProvider, pinned_hash: Vec<u8>) -> Self {
        Self {
            provider,
            pinned_hash,
        }
    }
}
impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let mut raw_certs = vec![end_entity.as_ref()];
        for cert in intermediates {
            raw_certs.push(cert.as_ref());
        }
        let computed_hash = juicity_common::crypto::generate_cert_chain_hash(&raw_certs);
        if computed_hash == self.pinned_hash {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "pinned cert chain hash mismatch".to_string(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_client() -> JuicityClient {
        JuicityClient::new(&Config {
            server: "127.0.0.1:443".to_string(),
            sni: "invalid sni".to_string(),
            uuid: "12345678-1234-1234-1234-123456789abc".to_string(),
            password: "test-password".to_string(),
            allow_insecure: true,
            ..Config::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn resolved_addresses_try_failure_then_success() {
        let unavailable = tokio::net::TcpSocket::new_v4().unwrap();
        unavailable.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reachable = listener.local_addr().unwrap();
        let stream = try_addresses(
            [unavailable.local_addr().unwrap(), reachable],
            |addr| async move { Ok(tokio::net::TcpStream::connect(addr).await?) },
        )
        .await
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), reachable);
    }

    #[tokio::test]
    async fn shared_error_keeps_quic_cause_text() {
        let client = test_client().await;
        client.reconnect_lock.lock().await.last_failure = Some((
            std::time::Instant::now(),
            Arc::new(anyhow::Error::new(quinn::ConnectionError::TimedOut)
                .context("QUIC handshake failed")),
        ));
        let error = client.connect().await.unwrap_err();
        assert_eq!(format!("{error:#}"), "QUIC handshake failed: timed out");
    }

    #[tokio::test]
    async fn eager_failure_does_not_back_off_real_requests() {
        let client = test_client().await;
        assert!(client.preconnect().await.is_err());
        {
            let reconnect = client.reconnect_lock.lock().await;
            assert!(reconnect.last_failure.is_none());
            assert_eq!(reconnect.attempts, 0);
        }
        assert!(client.connect().await.is_err());
        let reconnect = client.reconnect_lock.lock().await;
        assert!(reconnect.last_failure.is_some());
        assert_eq!(reconnect.attempts, 1);
    }

    #[tokio::test]
    async fn stalled_dns_lookup_times_out() {
        let error = resolve_with_timeout(
            std::future::pending::<std::io::Result<()>>(),
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("DNS lookup timed out"));
    }

    #[tokio::test]
    async fn udp_payload_length_boundary_and_early_rejection() {
        let data = vec![0; u16::MAX as usize + 1];
        assert_eq!(udp_packet_len(&[]).unwrap(), [0, 0]);
        assert_eq!(udp_packet_len(&data[..u16::MAX as usize]).unwrap(), [255, 255]);
        assert!(udp_packet_len(&data).is_err());
        let client = test_client().await;
        let error = client.open_udp_stream("127.0.0.1", 53, &data).await.unwrap_err();
        assert_eq!(error.to_string(), "UDP payload exceeds 65535 bytes");
        assert_eq!(client.reconnect_lock.lock().await.attempts, 0);
    }

    #[test]
    fn server_address_and_sni() {
        for (server, override_sni, host, port, sni) in [
            ("proxy.example:443", "", "proxy.example", 443, "proxy.example"),
            (
                "proxy.example:8443",
                "tls.example",
                "proxy.example",
                8443,
                "tls.example",
            ),
            ("127.0.0.1:443", "", "127.0.0.1", 443, "127.0.0.1"),
            ("127.0.0.1:443", "tls.example", "127.0.0.1", 443, "tls.example"),
            ("[2001:db8::1]:443", "", "2001:db8::1", 443, "2001:db8::1"),
            ("[::1]:8443", "tls.example", "::1", 8443, "tls.example"),
        ] {
            assert_eq!(
                parse_server(server, override_sni).unwrap(),
                (host.to_string(), port, sni.to_string())
            );
        }
        for server in [
            "proxy.example",
            "proxy.example:",
            "proxy.example:65536",
            "proxy.example:abc",
            ":443",
            "[::1]",
            "::1:443",
            "[localhost]:443",
        ] {
            assert!(parse_server(server, "").is_err(), "accepted {server}");
        }
    }

    #[tokio::test]
    async fn reconnect_failure_is_shared_until_backoff_expires() {
        let config = Config {
            server: "127.0.0.1:443".to_string(),
            sni: "invalid sni".to_string(),
            uuid: "12345678-1234-1234-1234-123456789abc".to_string(),
            password: "test-password".to_string(),
            allow_insecure: true,
            ..Config::default()
        };
        let client = JuicityClient::new(&config).await.unwrap();
        let guard = client.reconnect_lock.lock().await;
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let client = client.clone();
            callers.spawn(async move { client.connect().await.unwrap_err().to_string() });
        }
        tokio::task::yield_now().await;
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let error = callers.join_next().await.unwrap().unwrap();
            assert!(error.contains("invalid"), "{error}");
            while let Some(result) = callers.join_next().await {
                assert_eq!(result.unwrap(), error);
            }
        })
        .await
        .expect("queued callers must not sleep through backoff");
        let mut reconnect = client.reconnect_lock.lock().await;
        assert_eq!(reconnect.attempts, 1);

        reconnect.attempts = u32::MAX;
        assert_eq!(
            reconnect_delay(reconnect.attempts),
            std::time::Duration::from_secs(30)
        );
        reconnect.last_failure.as_mut().unwrap().0 =
            std::time::Instant::now() - std::time::Duration::from_secs(31);
        drop(reconnect);
        assert!(client
            .connect()
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid"));
        let reconnect = client.reconnect_lock.lock().await;
        assert_eq!(reconnect.attempts, u32::MAX);
        assert!(
            reconnect.last_failure.as_ref().unwrap().0.elapsed() < std::time::Duration::from_secs(1)
        );
    }
}
