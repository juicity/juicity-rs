use std::net::SocketAddr;
use std::sync::Arc;

use juicity_common::consts;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, RecvStream, SendStream, VarInt};
use uuid::Uuid;

/// A Juicity client that connects to a remote Juicity server
#[derive(Clone)]
pub struct JuicityClient {
    endpoint: Arc<Endpoint>,
    server_addr: SocketAddr,
    uuid: Uuid,
    password: zeroize::Zeroizing<String>,
    sni: String,
    quic_config: Arc<ClientConfig>,
    conn: Arc<tokio::sync::RwLock<Option<Connection>>>,
    auth_uni_stream: Arc<tokio::sync::Mutex<Option<SendStream>>>,
    /// Serialises reconnection: only one task may execute the slow reconnect path at a time.
    reconnect_lock: Arc<tokio::sync::Mutex<()>>,
    /// Tracks the last reconnection failure time to implement backoff.
    /// Prevents busy-looping when the server is down.
    last_reconnect_failure: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
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

        // Set keep_alive_interval if configured; otherwise use default
        let keep_alive = keep_alive_interval
            .map(std::time::Duration::from_secs)
            .unwrap_or(consts::KEEP_ALIVE_PERIOD);
        transport_config.keep_alive_interval(Some(keep_alive));

        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config
            .max_concurrent_uni_streams(VarInt::from_u32(consts::MAX_OPEN_INCOMING_STREAMS as u32));
        // Set an explicit idle timeout for defense-in-depth.
        // Even with keep-alive enabled, if the peer stops responding or never opens
        // a stream after authentication, this timeout ensures the connection and its
        // associated resources (auth reader task, Arc references) are eventually released.
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
                // Low latency: reduce window to save memory
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW / 2,
                ));
                transport_config
                    .receive_window(VarInt::from_u32(consts::QUIC_CONNECTION_RECEIVE_WINDOW / 2));
            } else if rtt_ms > 200 {
                // High latency: increase window to improve throughput
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
                // Tune BBR parameters: set a reasonable initial window to balance latency and throughput
                let mut bbr_config = quinn::congestion::BbrConfig::default();
                // Set initial congestion window (in bytes)
                // Default is min(10*MTU, max(2*MTU, 14720)), here adjusted to 10*MTU
                bbr_config.initial_window(10 * consts::ETHERNET_MTU as u64);
                transport_config.congestion_controller_factory(Arc::new(bbr_config))
            }
        };
        quic_config.transport_config(Arc::new(transport_config));

        Ok(quic_config)
    }

    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let uuid = Uuid::parse_str(&config.uuid)?;
        let server_addr: SocketAddr = config.server.parse()?;
        let sni = if config.sni.is_empty() {
            server_addr.ip().to_string()
        } else {
            config.sni.clone()
        };

        let pinned_hash = if config.pinned_certchain_sha256.is_empty() {
            Vec::new()
        } else {
            // Try base64 (URL-safe first, then standard), fall back to hex.
            // This trial-and-error approach is used for compatibility with various
            // configuration formats; ideally a format prefix (e.g. "base64:"/"hex:")
            // should be used to disambiguate.
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

        // Endpoint::client() may perform synchronous DNS resolution and socket
        // operations internally. Run it in spawn_blocking to avoid blocking
        // the async runtime during startup.
        let bind_addr: SocketAddr = "[::]:0".parse()?;

        let endpoint = if let Some(fwmark) = config.fwmark {
            // When fwmark is set, manually create the socket with socket2 so we
            // can set SO_MARK before handing it to Quinn.
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use socket2::{Domain, Protocol, Socket, Type};

                // Create IPv6 dual-stack socket
                let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;

                // Enable dual-stack (IPv4-mapped IPv6)
                sock.set_only_v6(false)?;

                // Set fwmark (SO_MARK, Linux only)
                #[cfg(target_os = "linux")]
                sock.set_mark(fwmark)?;

                // Non-Linux platforms: fwmark is not supported, warn the user
                #[cfg(not(target_os = "linux"))]
                println!(
                    "Warning: fwmark is only supported on Linux, ignoring fwmark={}",
                    fwmark
                );

                // Bind to address
                sock.bind(&bind_addr.into())?;

                // Convert to std UdpSocket
                let std_socket: std::net::UdpSocket = sock.into();

                // Wrap with quinn runtime
                let runtime = quinn::default_runtime()
                    .ok_or_else(|| anyhow::anyhow!("No quinn runtime available"))?;
                let wrapped = runtime.wrap_udp_socket(std_socket)?;

                // Create Endpoint with the wrapped socket
                let endpoint = Endpoint::new_with_abstract_socket(
                    EndpointConfig::default(),
                    None, // client does not need ServerConfig
                    wrapped,
                    runtime,
                )?;

                Ok(endpoint)
            })
            .await??
        } else {
            // Original logic: use Endpoint::client directly
            tokio::task::spawn_blocking(move || Endpoint::client(bind_addr)).await??
        };
        let endpoint = Arc::new(endpoint);

        // Build and cache the QUIC client config once.
        // TLS config construction is CPU-bound (certificate parsing, crypto setup).
        // Run it in spawn_blocking to avoid blocking the async runtime.
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
            server_addr,
            uuid,
            password: zeroize::Zeroizing::new(config.password.clone()),
            sni,
            quic_config,
            conn: Arc::new(tokio::sync::RwLock::new(None)),
            auth_uni_stream: Arc::new(tokio::sync::Mutex::new(None)),
            reconnect_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_reconnect_failure: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Connect to the server and authenticate using TLS ExportKeyingMaterial
    pub async fn connect(&self) -> anyhow::Result<Connection> {
        // Fast path: read lock allows concurrent callers to check a live connection
        // in parallel without blocking each other.
        {
            let guard = self.conn.read().await;
            if let Some(conn) = guard.as_ref() {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
        }

        // Slow path: serialize reconnection so only one task reconnects at a time.
        // Without this lock, multiple tasks that all fail the fast-path check would
        // all race to reconnect simultaneously, leaking connections and corrupting
        // auth_uni_stream state.
        let _reconnect_guard = self.reconnect_lock.lock().await;

        // Exponential backoff: if the last connection attempt failed recently,
        // wait at least 1 second before retrying to avoid busy-looping.
        {
            let last_failure = self.last_reconnect_failure.lock().await;
            if let Some(last) = *last_failure {
                let elapsed = last.elapsed();
                if elapsed < std::time::Duration::from_secs(1) {
                    tokio::time::sleep(std::time::Duration::from_secs(1) - elapsed).await;
                }
            }
        }

        // Double-check: another task may have already reconnected while we waited.
        {
            let guard = self.conn.read().await;
            if let Some(conn) = guard.as_ref() {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
        }

        {
            let mut guard = self.conn.write().await;
            *guard = None;
        }
        {
            let mut auth_guard = self.auth_uni_stream.lock().await;
            *auth_guard = None;
        }

        tracing::info!("Connecting to Juicity server at {}", self.server_addr);

        // Wrap the fallible connection + auth logic so we can record failures
        // before returning the error.
        let connect_result = (async {
            let addr = SocketAddr::new(self.server_addr.ip(), self.server_addr.port());
            let quinn_conn = self
                .endpoint
                .connect_with((*self.quic_config).clone(), addr, &self.sni)?
                .await?;

            // === Authenticate (compatible with upstream) ===
            // Format: [version=0][cmd_type=Authenticate(0x00)][uuid(16)][token(32)]
            let mut uni = quinn_conn.open_uni().await?;

            // Token using TLS ExportKeyingMaterial(uuid, password, 32) per RFC 5705.
            // export_keying_material is CPU-bound (HKDF); run it in spawn_blocking to
            // avoid occupying the async event loop on every connection attempt.
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

            // Batch all 50 auth bytes into a single write using a fixed-size
            // stack array to avoid a Vec heap allocation per connection.
            // [version(1)][cmd_type(1)][uuid(16)][token(32)]
            let mut auth_buf = [0u8; 50];
            auth_buf[0] = protocol::PROTOCOL_VERSION;
            auth_buf[1] = protocol::AUTHENTICATE_TYPE;
            auth_buf[2..18].copy_from_slice(self.uuid.as_bytes());
            auth_buf[18..50].copy_from_slice(&token);
            uni.write_all(&auth_buf).await?;

            anyhow::Ok((quinn_conn, uni))
        })
        .await;

        let (quinn_conn, uni) = match connect_result {
            Ok(pair) => pair,
            Err(e) => {
                // Record the failure time for exponential backoff on the next retry.
                *self.last_reconnect_failure.lock().await = Some(std::time::Instant::now());
                return Err(e);
            }
        };

        tracing::info!("Authenticated as user {}", self.uuid);

        {
            let mut guard = self.conn.write().await;
            *guard = Some(quinn_conn.clone());
        }
        {
            let mut auth_guard = self.auth_uni_stream.lock().await;
            *auth_guard = Some(uni);
        }

        Ok(quinn_conn)
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
        let conn = self.connect().await?;
        let (mut send, recv) = conn.open_bi().await?;

        // Batch stream header + first datagram into a single write to reduce async round-trips:
        //   stream header:  [network=3][trojanc_addr]
        //   first datagram: [trojanc_addr][len(2)][payload]
        let stream_header = protocol::build_proxy_header(protocol::NETWORK_UDP, addr, port)?;
        let dgram_addr = protocol::build_trojanc_addr(addr, port)?;
        let pkt_len = (first_packet.len() as u16).to_be_bytes();
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
    pub async fn send_udp_datagram(
        send: &mut SendStream,
        addr: &str,
        port: u16,
        data: &[u8],
        addr_buf: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let cached = protocol::CachedAddr::from_host_port(addr, port);
        addr_buf.clear();
        protocol::build_trojanc_addr_cached(addr_buf, &cached)?;
        send.write_all(addr_buf).await?;
        let len = (data.len() as u16).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(data).await?;
        Ok(())
    }
}

// ============= Certificate verifiers =============

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
