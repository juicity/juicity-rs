use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use juicity_common::consts;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{Endpoint, EndpointConfig, RecvStream, SendStream, VarInt};
use uuid::Uuid;

#[derive(Clone)]
struct UnderlaySession {
    target: String,
    psk: Vec<u8>,
}

/// Juicity proxy server
pub struct JuicityServer {
    users: Arc<HashMap<Uuid, String>>,
    server_config: quinn::ServerConfig,
    dialer: Arc<dyn crate::dialer::Dialer>,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_endpoint_pool: Arc<crate::udp::UdpEndpointPool>,
    disable_outbound_udp443: bool,
}

impl JuicityServer {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let mut users = HashMap::new();
        for (id, password) in &config.users {
            let uuid = Uuid::parse_str(id)?;
            users.insert(uuid, password.clone());
        }

        let certs = load_certs(&config.certificate)?;
        let key = load_private_key(&config.private_key)?;

        let mut tls_server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;

        // Juicity spec requires ALPN to be h3.
        tls_server_config.alpn_protocols = vec![b"h3".to_vec()];

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls_server_config)?,
        ));

        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config.max_concurrent_uni_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config.keep_alive_interval(Some(consts::KEEP_ALIVE_PERIOD));
        // Enable BBR congestion control (compatible with Go version)
        transport_config.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        server_config.transport_config(Arc::new(transport_config));

        let dialer: Arc<dyn crate::dialer::Dialer> = if !config.send_through.is_empty() {
            let addr: std::net::IpAddr = config.send_through.parse()?;
            Arc::new(crate::dialer::BindDialer { bind_addr: addr })
        } else {
            Arc::new(crate::dialer::DefaultDialer)
        };

        Ok(Self {
            users: Arc::new(users),
            server_config,
            dialer,
            in_flight: Arc::new(crate::inflight::InFlightUnderlayKey::new(
                consts::IN_FLIGHT_UNDERLAY_TTL,
            )),
            udp_endpoint_pool: Arc::new(crate::udp::UdpEndpointPool::new()),
            disable_outbound_udp443: config.disable_outbound_udp443,
        })
    }

    pub async fn serve(&self, addr: &str) -> anyhow::Result<()> {
        // Support ":port" shorthand (e.g. ":23182") — bind to all interfaces
        let addr = if addr.starts_with(':') {
            format!("0.0.0.0{}", addr)
        } else {
            addr.to_string()
        };
        let socket_addr: SocketAddr = addr.parse()?;

        let udp_socket = std::net::UdpSocket::bind(socket_addr)?;
        udp_socket.set_nonblocking(true)?;
        let sidecar_socket = udp_socket.try_clone()?;
        sidecar_socket.set_nonblocking(true)?;
        let server_underlay_socket = Arc::new(tokio::net::UdpSocket::from_std(sidecar_socket)?);

        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow::anyhow!("no async runtime found for quinn"))?;
        let wrapped_socket = runtime.wrap_udp_socket(udp_socket)?;

        let (underlay_tx, underlay_rx) = tokio::sync::mpsc::unbounded_channel();
        let demux_socket = Arc::new(crate::underlay_socket::DemuxUdpSocket::new(
            wrapped_socket,
            underlay_tx,
        ));

        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(self.server_config.clone()),
            demux_socket,
            runtime,
        )?;

        tracing::info!("Juicity server listening on {}", addr);

        // Spawn periodic cleanup task for in-flight underlay keys
        let inflight_cleanup = self.in_flight.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                inflight_cleanup.cleanup();
            }
        });

        // Spawn periodic cleanup task for UDP endpoint pool
        let udp_pool_cleanup = self.udp_endpoint_pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                udp_pool_cleanup.cleanup();
            }
        });

        let underlay_in_flight = self.in_flight.clone();
        let underlay_udp_pool = self.udp_endpoint_pool.clone();
        let underlay_disable_443 = self.disable_outbound_udp443;
        let underlay_socket = server_underlay_socket.clone();
        tokio::spawn(async move {
            run_underlay_packet_loop(
                underlay_rx,
                underlay_in_flight,
                underlay_udp_pool,
                underlay_socket,
                underlay_disable_443,
            )
            .await;
        });

        while let Some(incoming) = endpoint.accept().await {
            let users = self.users.clone();
            let in_flight = self.in_flight.clone();
            let udp_pool = self.udp_endpoint_pool.clone();
            let dialer = self.dialer.clone();
            let disable_443 = self.disable_outbound_udp443;

            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(incoming, users, in_flight, udp_pool, dialer, disable_443)
                        .await
                {
                    tracing::warn!("Connection handler error: {:?}", e);
                }
            });
        }
        Ok(())
    }
}

async fn run_underlay_packet_loop(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::underlay_socket::UnderlayPacket>,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    server_socket: Arc<tokio::net::UdpSocket>,
    disable_udp_443: bool,
) {
    let sessions: Arc<tokio::sync::Mutex<HashMap<SocketAddr, UnderlaySession>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(packet) = rx.recv().await {
        if let Err(e) = handle_non_quic_underlay_packet(
            packet,
            in_flight.clone(),
            udp_pool.clone(),
            sessions.clone(),
            server_socket.clone(),
            disable_udp_443,
        )
        .await
        {
            tracing::debug!("non-QUIC underlay packet handling failed: {:?}", e);
        }
    }
}

async fn handle_non_quic_underlay_packet(
    packet: crate::underlay_socket::UnderlayPacket,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    sessions: Arc<tokio::sync::Mutex<HashMap<SocketAddr, UnderlaySession>>>,
    server_socket: Arc<tokio::net::UdpSocket>,
    disable_udp_443: bool,
) -> anyhow::Result<()> {
    if packet.payload.len() < consts::UNDERLAY_SALT_LEN {
        return Ok(());
    }

    let source = packet.peer;
    let existing_session = {
        let guard = sessions.lock().await;
        guard.get(&source).cloned()
    };

    let (session, plaintext) = if let Some(existing) = existing_session {
        let plain = match juicity_common::crypto::juicity_underlay::decrypt_udp(
            &existing.psk,
            &packet.payload,
        ) {
            Ok(plain) => plain,
            Err(e) => {
                tracing::debug!(
                    "drop invalid underlay packet from {} for target {}: {:?}",
                    source,
                    existing.target,
                    e
                );
                return Ok(());
            }
        };
        (existing, plain)
    } else {
        let mut salt = [0u8; consts::UNDERLAY_SALT_LEN];
        salt.copy_from_slice(&packet.payload[..consts::UNDERLAY_SALT_LEN]);
        let auth = match in_flight.evict(&salt).await {
            Some(auth) => auth,
            None => {
                tracing::debug!(
                    "drop non-QUIC packet from {}: missing in-flight underlay auth",
                    source
                );
                return Ok(());
            }
        };

        if disable_udp_443 && auth.metadata.port == 443 {
            tracing::debug!("blocked underlay UDP/443: {}", auth.metadata.target_addr());
            return Ok(());
        }

        let session = UnderlaySession {
            target: auth.metadata.target_addr(),
            psk: auth.psk,
        };
        let plaintext = match juicity_common::crypto::juicity_underlay::decrypt_udp(
            &session.psk,
            &packet.payload,
        ) {
            Ok(plain) => plain,
            Err(e) => {
                tracing::debug!(
                    "drop first underlay packet from {} for target {}: {:?}",
                    source,
                    session.target,
                    e
                );
                return Ok(());
            }
        };

        {
            let mut guard = sessions.lock().await;
            guard.insert(source, session.clone());
        }
        tracing::debug!("new underlay session {} -> {}", source, session.target);
        (session, plaintext)
    };

    let ((udp_socket, dial_target), is_new) = udp_pool
        .get_or_create(
            source,
            crate::udp::UdpEndpointOptions {
                // Response path is handled by the dedicated reader spawned below.
                handler: Box::new(|_, _| Ok(())),
                nat_timeout: Duration::from_secs(180),
                dial_target: session.target,
            },
        )
        .await?;

    if is_new {
        let recv_socket = tokio::net::UdpSocket::from_std(udp_socket.try_clone()?)?;
        let relay_back = server_socket.clone();
        let session_psk = session.psk.clone();
        let sessions_for_task = sessions.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; consts::ETHERNET_MTU * 4];
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((n, _)) => {
                        let salt = juicity_common::crypto::juicity_underlay::generate_underlay_salt();
                        let encrypted = match juicity_common::crypto::juicity_underlay::encrypt_udp(
                            &session_psk,
                            &buf[..n],
                            &salt,
                        ) {
                            Ok(encrypted) => encrypted,
                            Err(e) => {
                                tracing::debug!(
                                    "underlay response encrypt failed for {}: {:?}",
                                    source,
                                    e
                                );
                                break;
                            }
                        };

                        if relay_back.send_to(&encrypted, source).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("underlay endpoint recv failed for {}: {:?}", source, e);
                        break;
                    }
                }
            }

            let mut guard = sessions_for_task.lock().await;
            guard.remove(&source);
        });
    }

    let send_socket = tokio::net::UdpSocket::from_std(udp_socket)?;
    if let Err(e) = send_socket.send_to(&plaintext, &dial_target).await {
        udp_pool.remove(&source).await;
        let mut guard = sessions.lock().await;
        guard.remove(&source);
        return Err(anyhow::anyhow!("underlay send_to {} failed: {:?}", dial_target, e));
    }

    Ok(())
}

/// Handle an incoming QUIC connection
async fn handle_connection(
    incoming: quinn::Incoming,
    users: Arc<HashMap<Uuid, String>>,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    _udp_pool: Arc<crate::udp::UdpEndpointPool>,
    dialer: Arc<dyn crate::dialer::Dialer>,
    disable_udp_443: bool,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    let remote_addr = connection.remote_address();
    tracing::debug!("New QUIC connection from {}", remote_addr);

    // === Authenticate ===
    let auth_conn = connection.clone();
    let auth_users = users.clone();

    let auth_result = tokio::time::timeout(consts::AUTHENTICATE_TIMEOUT, async {
        handle_auth(&auth_conn, auth_users).await
    })
    .await;

    let (user_uuid, mut auth_uni_stream) = match auth_result {
        Ok(Ok((uuid, stream))) => {
            tracing::debug!("User {} authenticated from {}", uuid, remote_addr);
            (uuid, stream)
        }
        Ok(Err(e)) => {
            tracing::warn!("Authentication failed from {}: {:?}", remote_addr, e);
            connection.close(VarInt::from_u32(0xfffffff1), b"authentication failed");
            return Err(e);
        }
        Err(_) => {
            connection.close(VarInt::from_u32(0xfffffff2), b"authentication timeout");
            return Err(anyhow::anyhow!("auth timeout"));
        }
    };

    // Keep reading underlay auth entries from the authenticated uni stream.
    let in_flight_for_auth = in_flight.clone();
    tokio::spawn(async move {
        loop {
            match protocol::read_underlay_auth_async(&mut auth_uni_stream).await {
                Ok(auth) => {
                    in_flight_for_auth.store(auth.iv, auth);
                }
                Err(e) => {
                    tracing::debug!(
                        "Underlay auth stream closed for {}: {:?}",
                        user_uuid,
                        e
                    );
                    break;
                }
            }
        }
    });

    // Accept and handle streams
    loop {
        match connection.accept_bi().await {
            Ok((send_stream, recv_stream)) => {
                let s_dialer = dialer.clone();
                let s_user_uuid = user_uuid;
                let s_disable_443 = disable_udp_443;

                tokio::spawn(async move {
                    if let Err(e) = handle_stream(
                        send_stream,
                        recv_stream,
                        s_dialer,
                        s_user_uuid,
                        s_disable_443,
                    )
                    .await
                    {
                        tracing::debug!("Stream handler error: {:?}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                tracing::debug!("Connection closed by peer: {}", remote_addr);
                break;
            }
            Err(e) => {
                tracing::debug!("Accept stream error: {:?}", e);
                break;
            }
        }
    }
    Ok(())
}

/// Handle authentication via unidirectional stream
/// Format: [version=0][cmd_type=Authenticate(0x00)][uuid(16)][token(32)]
async fn handle_auth(
    conn: &quinn::Connection,
    users: Arc<HashMap<Uuid, String>>,
) -> anyhow::Result<(Uuid, RecvStream)> {
    let mut uni_stream = conn.accept_uni().await?;

    let (version, cmd_type) = protocol::read_command_head_async(&mut uni_stream).await?;
    if version != protocol::PROTOCOL_VERSION {
        anyhow::bail!("unsupported protocol version: {}", version);
    }
    if cmd_type != protocol::AUTHENTICATE_TYPE {
        anyhow::bail!("expected authenticate command, got: {}", cmd_type);
    }

    let (uuid, received_token) = protocol::read_authenticate_async(&mut uni_stream).await?;

    let password = users
        .get(&uuid)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown user: {}", uuid))?;

    // Verify token using TLS ExportKeyingMaterial (RFC 5705) - same as upstream
    let expected_token = protocol::gen_token_via_connection(conn, &uuid, &password)?;

    if expected_token == received_token {
        tracing::debug!("User {} authenticated successfully", uuid);
        Ok((uuid, uni_stream))
    } else {
        tracing::warn!("Token mismatch for user {}", uuid);
        Err(anyhow::anyhow!("token mismatch for user {}", uuid))
    }
}

/// Handle a bidirectional stream - read proxy header, relay TCP or UDP
async fn handle_stream(
    send_stream: SendStream,
    mut recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    user_uuid: Uuid,
    disable_udp_443: bool,
) -> anyhow::Result<()> {
    // Read proxy header via async reader
    let (network, hostname, port) = protocol::read_proxy_header_async(&mut recv_stream).await?;
    let target = format!("{}:{}", hostname, port);

    match network {
        protocol::NETWORK_TCP => {
            tracing::debug!("TCP relay: {} -> {}", user_uuid, target);
            handle_tcp_relay(send_stream, recv_stream, dialer, &target).await
        }
        protocol::NETWORK_UDP => {
            if disable_udp_443 && port == 443 {
                tracing::debug!("Blocked UDP/443: {}", target);
                return Ok(());
            }
            tracing::debug!("UDP relay: {} -> {}", user_uuid, target);
            handle_udp_relay(send_stream, recv_stream, dialer, &hostname, port).await
        }
        _ => anyhow::bail!("unknown network type: {}", network),
    }
}

/// TCP relay: bidirectional copy between QUIC stream and remote TCP
async fn handle_tcp_relay(
    send_stream: SendStream,
    recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    target: &str,
) -> anyhow::Result<()> {
    let remote = dialer.dial_tcp(target).await?;
    let (mut remote_rx, mut remote_tx) = tokio::io::split(remote);
    let (mut quic_tx, mut quic_rx) = (send_stream, recv_stream);

    tokio::select! {
        r = tokio::io::copy(&mut remote_rx, &mut quic_tx) => {
            if let Err(e) = r { tracing::debug!("TCP relay remote->quic: {:?}", e); }
        }
        r = tokio::io::copy(&mut quic_rx, &mut remote_tx) => {
            if let Err(e) = r { tracing::debug!("TCP relay quic->remote: {:?}", e); }
        }
    }
    Ok(())
}

/// UDP over Stream relay.
/// Per spec: each UDP datagram = [proxy_header(Network=UDP=3)][len(2)][payload]
async fn handle_udp_relay(
    mut send_stream: SendStream,
    mut recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    hostname: &str,
    port: u16,
) -> anyhow::Result<()> {
    let mut domain_ip_map: HashMap<(String, u16), SocketAddr> = HashMap::new();
    let first_target_addr = resolve_udp_target(hostname, port, &mut domain_ip_map).await?;

    let remote = dialer.dial_udp(&first_target_addr.to_string()).await?;
    let remote = Arc::new(remote);

    // Read first datagram payload (header already consumed)
    let mut len_buf = [0u8; 2];
    recv_stream.read_exact(&mut len_buf).await?;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; pkt_len];
    recv_stream.read_exact(&mut data).await?;
    remote.send_to(&data, first_target_addr).await?;

    // Bidirectional relay
    let quic_to_remote = {
        let remote = remote.clone();
        let mut domain_ip_map = domain_ip_map;
        tokio::spawn(async move {
            loop {
                // Each subsequent datagram: [proxy_header][len(2)][payload]
                let mut network = [0u8; 1];
                if recv_stream.read_exact(&mut network).await.is_err() {
                    break;
                }
                if network[0] != protocol::NETWORK_UDP {
                    break;
                }

                let mut addr_type = [0u8; 1];
                if recv_stream.read_exact(&mut addr_type).await.is_err() {
                    break;
                }

                let (t_addr, t_port) = match addr_type[0] {
                    protocol::ADDR_TYPE_NONE => {
                        // Skip reading addr, keep current target
                        ("".to_string(), 0u16)
                    }
                    protocol::ADDR_TYPE_IPV4 => {
                        let mut ip = [0u8; 4];
                        if recv_stream.read_exact(&mut ip).await.is_err() {
                            break;
                        }
                        let mut pb = [0u8; 2];
                        if recv_stream.read_exact(&mut pb).await.is_err() {
                            break;
                        }
                        (
                            std::net::Ipv4Addr::from(ip).to_string(),
                            u16::from_be_bytes(pb),
                        )
                    }
                    protocol::ADDR_TYPE_IPV6 => {
                        let mut ip = [0u8; 16];
                        if recv_stream.read_exact(&mut ip).await.is_err() {
                            break;
                        }
                        let mut pb = [0u8; 2];
                        if recv_stream.read_exact(&mut pb).await.is_err() {
                            break;
                        }
                        (
                            std::net::Ipv6Addr::from(ip).to_string(),
                            u16::from_be_bytes(pb),
                        )
                    }
                    protocol::ADDR_TYPE_DOMAIN => {
                        let mut lb = [0u8; 1];
                        if recv_stream.read_exact(&mut lb).await.is_err() {
                            break;
                        }
                        let dlen = lb[0] as usize;
                        let mut domain = vec![0u8; dlen];
                        if recv_stream.read_exact(&mut domain).await.is_err() {
                            break;
                        }
                        let mut pb = [0u8; 2];
                        if recv_stream.read_exact(&mut pb).await.is_err() {
                            break;
                        }
                        (
                            String::from_utf8_lossy(&domain).to_string(),
                            u16::from_be_bytes(pb),
                        )
                    }
                    _ => break,
                };

                let mut len_bytes = [0u8; 2];
                if recv_stream.read_exact(&mut len_bytes).await.is_err() {
                    break;
                }
                let pkt_len = u16::from_be_bytes(len_bytes) as usize;
                let mut payload = vec![0u8; pkt_len];
                if recv_stream.read_exact(&mut payload).await.is_err() {
                    break;
                }

                let target = if t_addr.is_empty() {
                    first_target_addr
                } else {
                    match resolve_udp_target(&t_addr, t_port, &mut domain_ip_map).await {
                        Ok(addr) => addr,
                        Err(e) => {
                            tracing::debug!("UDP target resolve error: {:?}", e);
                            break;
                        }
                    }
                };
                if let Err(e) = remote.send_to(&payload, target).await {
                    tracing::debug!("UDP relay write error: {:?}", e);
                    break;
                }
            }
        })
    };

    let remote_to_quic = {
        let remote = remote.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; consts::ETHERNET_MTU];
            loop {
                match remote.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        // Relay back: proxy_header + len + payload
                        let addr_str = addr.ip().to_string();
                        let addr_port = addr.port();
                        if let Ok(hdr) =
                            protocol::build_proxy_header(protocol::NETWORK_UDP, &addr_str, addr_port)
                        {
                            if send_stream.write_all(&hdr).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                        let len = (n as u16).to_be_bytes();
                        if send_stream.write_all(&len).await.is_err() {
                            break;
                        }
                        if send_stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    tokio::select! {
        _ = quic_to_remote => {}
        _ = remote_to_quic => {}
    }
    Ok(())
}

async fn resolve_udp_target(
    host: &str,
    port: u16,
    domain_ip_map: &mut HashMap<(String, u16), SocketAddr>,
) -> anyhow::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    let key = (host.to_string(), port);
    if let Some(mapped) = domain_ip_map.get(&key) {
        return Ok(*mapped);
    }

    let mut addrs = tokio::net::lookup_host((host, port)).await?;
    let resolved = addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("no DNS result for {}:{}", host, port))?;
    domain_ip_map.insert(key, resolved);
    Ok(resolved)
}

fn load_certs(path: &str) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let f = std::fs::File::open(path)?;
    let mut r = std::io::BufReader::new(f);
    Ok(rustls_pemfile::certs(&mut r).collect::<Result<Vec<_>, _>>()?)
}

fn load_private_key(path: &str) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let f = std::fs::File::open(path)?;
    let mut r = std::io::BufReader::new(f);
    use rustls_pemfile::Item;

    // Find the first private key, avoiding unnecessary Vec allocation
    let mut key_count = 0u32;
    let first_key = loop {
        match rustls_pemfile::read_one(&mut r)? {
            Some(Item::Pkcs1Key(k)) => {
                key_count += 1;
                if key_count == 1 {
                    break Some(rustls::pki_types::PrivateKeyDer::Pkcs1(k));
                }
            }
            Some(Item::Pkcs8Key(k)) => {
                key_count += 1;
                if key_count == 1 {
                    break Some(rustls::pki_types::PrivateKeyDer::Pkcs8(k));
                }
            }
            Some(Item::Sec1Key(k)) => {
                key_count += 1;
                if key_count == 1 {
                    break Some(rustls::pki_types::PrivateKeyDer::Sec1(k));
                }
            }
            Some(_) => continue,
            None => break None,
        }
    };

    let key = first_key.ok_or_else(|| anyhow::anyhow!("no private key found in {}", path))?;

    if key_count > 1 {
        tracing::warn!(
            "multiple private keys ({}) found in {}, using the first one",
            key_count,
            path
        );
    }

    Ok(key)
}
