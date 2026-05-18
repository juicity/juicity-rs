use std::net::SocketAddr;
use std::sync::Arc;

use juicity_common::consts;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream};
use uuid::Uuid;

/// A Juicity client that connects to a remote Juicity server
#[derive(Clone)]
pub struct JuicityClient {
    endpoint: Arc<Endpoint>,
    server_addr: SocketAddr,
    uuid: Uuid,
    password: String,
    sni: String,
    quic_config: Arc<ClientConfig>,
    conn: Arc<tokio::sync::RwLock<Option<Connection>>>,
    auth_uni_stream: Arc<tokio::sync::Mutex<Option<SendStream>>>,
}

impl JuicityClient {
    /// Build a TLS client config based on the allow_insecure / pinned_certchain_sha256 settings.
    fn build_tls_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
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

        Ok(tls_config)
    }

    /// Build a QUIC client config (TLS + transport settings).
    fn build_quic_config(
        allow_insecure: bool,
        pinned_hash: &[u8],
        provider: &rustls::crypto::CryptoProvider,
    ) -> anyhow::Result<ClientConfig> {
        let tls_config = Self::build_tls_config(allow_insecure, pinned_hash, provider)?;

        let mut quic_config = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));

        let mut transport_config = quinn::TransportConfig::default();
        transport_config.keep_alive_interval(Some(consts::KEEP_ALIVE_PERIOD));
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
        let endpoint = tokio::task::spawn_blocking(move || {
            Endpoint::client(bind_addr)
        })
        .await??;
        let endpoint = Arc::new(endpoint);

        // Build and cache the QUIC client config once.
        // TLS config construction is CPU-bound (certificate parsing, crypto setup).
        // Run it in spawn_blocking to avoid blocking the async runtime.
        let allow_insecure = config.allow_insecure;
        let pinned_hash_for_config = pinned_hash.clone();
        let quic_config = tokio::task::spawn_blocking(move || {
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            Self::build_quic_config(allow_insecure, &pinned_hash_for_config, &provider)
        })
        .await??;
        let quic_config = Arc::new(quic_config);

        Ok(Self {
            endpoint,
            server_addr,
            uuid,
            password: config.password.clone(),
            sni,
            quic_config,
            conn: Arc::new(tokio::sync::RwLock::new(None)),
            auth_uni_stream: Arc::new(tokio::sync::Mutex::new(None)),
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

        // Slow path: write lock for reconnection.
        // Re-check after acquiring the write lock to avoid duplicate reconnections
        // when multiple tasks race to this point simultaneously.
        {
            let mut guard = self.conn.write().await;
            if let Some(conn) = guard.as_ref() {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
            *guard = None;
        }
        {
            let mut auth_guard = self.auth_uni_stream.lock().await;
            *auth_guard = None;
        }

        tracing::info!("Connecting to Juicity server at {}", self.server_addr);

        let addr = SocketAddr::new(self.server_addr.ip(), self.server_addr.port());
        let quinn_conn = self
            .endpoint
            .connect_with((*self.quic_config).clone(), addr, &self.sni)?
            .await?;

        // === Authenticate (compatible with upstream) ===
        // Format: [version=0][cmd_type=Authenticate(0x00)][uuid(16)][token(32)]
        let mut uni = quinn_conn.open_uni().await?;

        // Token using TLS ExportKeyingMaterial(uuid, password, 32) per RFC 5705
        let token = protocol::gen_token_via_connection(&quinn_conn, &self.uuid, &self.password)?;

        // Batch all 50 auth bytes into a single write to reduce async round-trips:
        // [version(1)][cmd_type(1)][uuid(16)][token(32)]
        let mut auth_buf = Vec::with_capacity(2 + 16 + 32);
        auth_buf.extend_from_slice(&[protocol::PROTOCOL_VERSION, protocol::AUTHENTICATE_TYPE]);
        auth_buf.extend_from_slice(self.uuid.as_bytes());
        auth_buf.extend_from_slice(&token);
        uni.write_all(&auth_buf).await?;

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
        let mut buf = Vec::with_capacity(
            stream_header.len() + dgram_addr.len() + 2 + first_packet.len(),
        );
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
    pub async fn send_udp_datagram(
        send: &mut SendStream,
        addr: &str,
        port: u16,
        data: &[u8],
    ) -> anyhow::Result<()> {
        let addr_header = protocol::build_trojanc_addr(addr, port)?;
        send.write_all(&addr_header).await?;
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
