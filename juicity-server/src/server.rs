use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use juicity_common::consts;

/// RAII guard: aborts the wrapped task when this guard is dropped.
/// Ensures background cleanup tasks do not outlive their owner.
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
use juicity_common::crypto::juicity_underlay;
use juicity_common::crypto::UnderlayCipher;
use juicity_common::protocol;
use juicity_common::Config;
use quinn::{Endpoint, EndpointConfig, RecvStream, SendStream, VarInt};
use uuid::Uuid;

#[derive(Clone)]
struct UnderlaySession {
    target: String,
    cipher: UnderlayCipher,
    /// Last time a packet was handled for this session (updated under the sessions lock).
    last_used: std::time::Instant,
    /// Abort handle for the relay-back task; `None` until the task is spawned.
    relay_abort: Option<tokio::task::AbortHandle>,
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

        // Load TLS certificates and private key via spawn_blocking to avoid
        // blocking the async runtime with synchronous file I/O.
        let cert_path = config.certificate.clone();
        let key_path = config.private_key.clone();
        let (certs, key) = tokio::try_join!(
            tokio::task::spawn_blocking(move || load_certs(&cert_path)),
            tokio::task::spawn_blocking(move || load_private_key(&key_path)),
        )?;
        let certs = certs?;
        let key = key?;

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
        // Support ":port" shorthand (e.g. ":23182").
        // When only a port is given, bind a dual-stack socket ([::]:port with
        // IPV6_V6ONLY=false) so both IPv4 and IPv6 clients are accepted on a
        // single socket.  An explicit address (e.g. "0.0.0.0:23182" or
        // "[::1]:23182") is parsed and used as-is.
        let (socket_addr, log_addr, dual_stack) = if addr.starts_with(':') {
            let v6: SocketAddr = format!("[::]{}", addr).parse()?;
            (v6, format!("[::]{} (dual-stack IPv4+IPv6)", addr), true)
        } else {
            let sa: SocketAddr = addr.parse()?;
            (sa, addr.to_string(), false)
        };

        // Build the raw UDP socket.  For dual-stack we use socket2 to clear
        // IPV6_V6ONLY before bind, which is necessary on Windows (Linux already
        // defaults to dual-stack but being explicit is safer).
        let udp_socket: std::net::UdpSocket = if dual_stack {
            use socket2::{Domain, Protocol, Socket, Type};
            let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            sock.set_only_v6(false)?;
            sock.bind(&socket_addr.into())?;
            std::net::UdpSocket::from(sock)
        } else {
            let tokio_udp = tokio::net::UdpSocket::bind(socket_addr).await?;
            tokio_udp.into_std()?
        };
        udp_socket.set_nonblocking(true)?;
        let sidecar_socket = udp_socket.try_clone()?;
        sidecar_socket.set_nonblocking(true)?;
        let server_underlay_socket = Arc::new(tokio::net::UdpSocket::from_std(sidecar_socket)?);

        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow::anyhow!("no async runtime found for quinn"))?;
        let wrapped_socket = runtime.wrap_udp_socket(udp_socket)?;

        let (underlay_tx, underlay_rx) = tokio::sync::mpsc::channel(
            crate::underlay_socket::UNDERLAY_CHANNEL_CAPACITY,
        );
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

        tracing::info!("Juicity server listening on {}", log_addr);

        // Spawn periodic cleanup task for in-flight underlay keys.
        // AbortOnDrop ensures the task is cancelled when serve() returns.
        let inflight_cleanup = self.in_flight.clone();
        let _inflight_guard = AbortOnDrop(
            tokio::spawn(async move {
                // Run at half the IN_FLIGHT_UNDERLAY_TTL interval to halve worst-case key residue.
                let mut interval = tokio::time::interval(consts::IN_FLIGHT_UNDERLAY_TTL / 2);
                loop {
                    interval.tick().await;
                    inflight_cleanup.cleanup();
                }
            })
            .abort_handle(),
        );

        // Spawn periodic cleanup task for UDP endpoint pool.
        let udp_pool_cleanup = self.udp_endpoint_pool.clone();
        let _pool_guard = AbortOnDrop(
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    udp_pool_cleanup.cleanup_async().await;
                }
            })
            .abort_handle(),
        );

        let underlay_in_flight = self.in_flight.clone();
        let underlay_udp_pool = self.udp_endpoint_pool.clone();
        let underlay_disable_443 = self.disable_outbound_udp443;
        let underlay_socket = server_underlay_socket.clone();
        // The underlay loop self-terminates when underlay_rx closes (endpoint drop),
        // but AbortOnDrop ensures it is also cancelled on any early serve() exit.
        let _underlay_guard = AbortOnDrop(
            tokio::spawn(async move {
                run_underlay_packet_loop(
                    underlay_rx,
                    underlay_in_flight,
                    underlay_udp_pool,
                    underlay_socket,
                    underlay_disable_443,
                )
                .await;
            })
            .abort_handle(),
        );

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
    mut rx: tokio::sync::mpsc::Receiver<crate::underlay_socket::UnderlayPacket>,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    server_socket: Arc<tokio::net::UdpSocket>,
    disable_udp_443: bool,
) {
    let sessions: Arc<std::sync::Mutex<HashMap<SocketAddr, UnderlaySession>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Periodic cleanup: remove sessions that have been idle for longer than the NAT
    // timeout and abort their relay-back tasks so they don't run indefinitely.
    // AbortOnDrop ensures this task is cancelled when run_underlay_packet_loop returns.
    let sessions_cleanup = sessions.clone();
    let _sessions_cleanup_guard = AbortOnDrop(
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                sessions_cleanup.lock().unwrap().retain(|_, s| {
                    if s.last_used.elapsed() >= consts::DEFAULT_NAT_TIMEOUT {
                        if let Some(h) = &s.relay_abort {
                            h.abort();
                        }
                        false
                    } else {
                        true
                    }
                });
            }
        })
        .abort_handle(),
    );

    // Limit concurrent underlay packet handler tasks to prevent unbounded task
    // accumulation under high traffic. Each handler may wait up to 100ms in
    // evict() for in-flight underlay auth, and without a cap, thousands of
    // tasks could pile up during a burst, consuming significant memory.
    // The limit is set to 2x the channel capacity so the channel backpressure
    // (drop at 1024 queued) and the semaphore work together as a two-layer throttle.
    let concurrency_limit = Arc::new(tokio::sync::Semaphore::new(
        crate::underlay_socket::UNDERLAY_CHANNEL_CAPACITY * 2,
    ));

    while let Some(packet) = rx.recv().await {
        // Acquire a permit before spawning. If all permits are taken (i.e. there
        // are already 2048 in-flight handler tasks), this await will back-pressure
        // the channel receiver, causing the DemuxUdpSocket's try_send to fail and
        // drop excess packets — a controlled degradation instead of unbounded growth.
        let permit = concurrency_limit.clone().acquire_owned().await;
        let in_flight = in_flight.clone();
        let udp_pool = udp_pool.clone();
        let sessions = sessions.clone();
        let server_socket = server_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_non_quic_underlay_packet(
                packet,
                in_flight,
                udp_pool,
                sessions,
                server_socket,
                disable_udp_443,
            )
            .await
            {
                tracing::debug!("non-QUIC underlay packet handling failed: {:?}", e);
            }
            // Drop the permit explicitly (though it will be dropped when the task
            // exits anyway) to release the slot for the next packet.
            drop(permit);
        });
    }
}

async fn handle_non_quic_underlay_packet(
    packet: crate::underlay_socket::UnderlayPacket,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    sessions: Arc<std::sync::Mutex<HashMap<SocketAddr, UnderlaySession>>>,
    server_socket: Arc<tokio::net::UdpSocket>,
    disable_udp_443: bool,
) -> anyhow::Result<()> {
    if packet.payload.len() < consts::UNDERLAY_SALT_LEN {
        return Ok(());
    }

    let source = packet.peer;
    // payload is already Vec<u8>; move it directly without copying.
    let mut payload = packet.payload;

    let existing_session = {
        let mut guard = sessions.lock().unwrap();
        if let Some(s) = guard.get_mut(&source) {
            s.last_used = std::time::Instant::now();
            Some(s.clone())
        } else {
            None
        }
    };

    let (session, pt_len) = if let Some(existing) = existing_session {
        // In-place decrypt using cached cipher
        if let Err(e) = existing.cipher.decrypt_in_place(&mut payload) {
            tracing::debug!(
                "drop invalid underlay packet from {} for target {}: {:?}",
                source,
                existing.target,
                e
            );
            return Ok(());
        }
        // After decrypt_in_place, payload contains only plaintext
        let pt_len = payload.len();
        (existing, pt_len)
    } else {
        let mut salt = [0u8; consts::UNDERLAY_SALT_LEN];
        salt.copy_from_slice(&payload[..consts::UNDERLAY_SALT_LEN]);
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

        // Derive subkey once and cache it in UnderlayCipher
        let subkey = juicity_underlay::derive_subkey(&auth.psk, &salt)?;
        let cipher = UnderlayCipher::from_subkey(&subkey);

        // In-place decrypt
        if let Err(e) = cipher.decrypt_in_place(&mut payload) {
            tracing::debug!(
                "drop first underlay packet from {} for target {}: {:?}",
                source,
                auth.metadata.target_addr(),
                e
            );
            return Ok(());
        }
        let pt_len = payload.len();

        let session = UnderlaySession {
            target: auth.metadata.target_addr(),
            cipher,
            last_used: std::time::Instant::now(),
            relay_abort: None,
        };

        {
            let mut guard = sessions.lock().unwrap();
            guard.insert(source, session.clone());
        }
        tracing::debug!("new underlay session {} -> {}", source, session.target);
        (session, pt_len)
    };

    let ((udp_socket, dial_target), is_new) = udp_pool
        .get_or_create(
            source,
            crate::udp::UdpEndpointOptions {
                nat_timeout: Duration::from_secs(180),
                dial_target: session.target.clone(),
            },
        )
        .await?;

    // Convert both sockets BEFORE any spawn so that a conversion failure
    // (e.g. kernel fd exhaustion) cannot produce an orphaned relay-back task.
    let recv_socket_for_relay = if is_new {
        Some(tokio::net::UdpSocket::from_std(udp_socket.try_clone()?)?)
    } else {
        None
    };
    let send_socket = tokio::net::UdpSocket::from_std(udp_socket)?;

    if let Some(recv_socket) = recv_socket_for_relay {
        let relay_back = server_socket.clone();
        let session_cipher = session.cipher.clone();
        let sessions_for_task = sessions.clone();
        let relay_handle = tokio::spawn(async move {
            // Pre-allocate full-capacity output buffer to avoid repeated Vec resizing.
            // Max payload: ETHERNET_MTU * 4, plus 32-byte salt prefix + 16-byte AEAD tag.
            let max_out_len = consts::ETHERNET_MTU * 4 + 48;
            let mut buf = vec![0u8; consts::ETHERNET_MTU * 4];
            let mut outbuf = Vec::with_capacity(max_out_len);
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((n, _)) => {
                        let salt = juicity_underlay::generate_underlay_salt();
                        // Pre-allocate exact capacity to avoid resize during encrypt_in_place
                        outbuf.clear();
                        outbuf.reserve(n + 48);
                        outbuf.extend_from_slice(&buf[..n]);
                        if session_cipher.encrypt_in_place(&mut outbuf, &salt).is_err() {
                            break;
                        }

                        if relay_back.send_to(&outbuf, source).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("underlay endpoint recv failed for {}: {:?}", source, e);
                        break;
                    }
                }
            }

            let mut guard = sessions_for_task.lock().unwrap();
            guard.remove(&source);
        });
        // Store the abort handle so periodic cleanup can cancel this task.
        // Use Entry API to avoid the race where relay_handle exits before we store the abort handle.
        let mut guard = sessions.lock().unwrap();
        match guard.entry(source) {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                o.get_mut().relay_abort = Some(relay_handle.abort_handle());
            }
            std::collections::hash_map::Entry::Vacant(_) => {
                // Session was already removed (e.g. relay task exited before we acquired the lock).
            }
        }
    }

    if let Err(e) = send_socket.send_to(&payload[..pt_len], &dial_target).await {
        udp_pool.remove(&source).await;
        // Remove the session and abort the relay task if one was spawned.
        let removed = sessions.lock().unwrap().remove(&source);
        if let Some(s) = removed {
            if let Some(h) = s.relay_abort {
                h.abort();
            }
        }
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
    // Store the abort handle so the task can be cancelled when the connection drops,
    // preventing the task (and its Arc references) from lingering indefinitely.
    let in_flight_for_auth = in_flight.clone();
    let auth_task_handle = tokio::spawn(async move {
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

    // Accept and handle streams.
    // JoinSet tracks all in-flight stream tasks: when dropped at connection end it
    // aborts any still-running streams, releasing their Arc<Dialer> and network
    // resources promptly instead of waiting for remote-side idle timeouts.
    let mut stream_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    loop {
        // Drain completed tasks each iteration to free their resources without blocking.
        while stream_tasks.try_join_next().is_some() {}

        match connection.accept_bi().await {
            Ok((send_stream, recv_stream)) => {
                let s_dialer = dialer.clone();
                let s_user_uuid = user_uuid;
                let s_disable_443 = disable_udp_443;

                stream_tasks.spawn(async move {
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
    // Abort the underlay auth reader task and all stream tasks so their Arc
    // references (in_flight, dialer, etc.) are released promptly.
    auth_task_handle.abort();
    // JoinSet drop aborts all remaining stream tasks automatically.
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
            tracing::debug!("UDP relay: {}", user_uuid);
            handle_udp_relay(send_stream, recv_stream, dialer, disable_udp_443).await
        }
        _ => anyhow::bail!("unknown network type: {}", network),
    }
}

/// TCP relay: bidirectional copy between QUIC stream and remote TCP.
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
/// UDP over Stream relay — upstream-compatible wire format.
///
/// Each UDP datagram on the stream (both directions):
///   [trojanc_addr][len(2)][payload]
/// No network byte per datagram; the stream header already carries it.
async fn handle_udp_relay(
    mut send_stream: SendStream,
    mut recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    disable_udp_443: bool,
) -> anyhow::Result<()> {
    let mut domain_ip_map: HashMap<(String, u16), SocketAddr> = HashMap::new();

    // First datagram: [trojanc_addr][len(2)][payload]
    let (first_host, first_port) = protocol::read_trojanc_addr_async(&mut recv_stream).await?;

    if disable_udp_443 && first_port == 443 {
        tracing::debug!("Blocked UDP/443: {}:{}", first_host, first_port);
        return Ok(());
    }

    let first_target_addr = resolve_udp_target(&first_host, first_port, &mut domain_ip_map).await?;
    let remote = dialer.dial_udp(&first_target_addr.to_string()).await?;
    let remote = Arc::new(remote);

    let mut len_buf = [0u8; 2];
    recv_stream.read_exact(&mut len_buf).await?;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; pkt_len];
    recv_stream.read_exact(&mut data).await?;
    remote.send_to(&data, first_target_addr).await?;

    // Bidirectional relay
    let mut quic_to_remote = {
        let remote = remote.clone();
        let mut domain_ip_map = domain_ip_map;
        tokio::spawn(async move {
            // Reuse a single buffer across all datagrams to avoid per-packet allocation.
            let mut payload = Vec::with_capacity(consts::ETHERNET_MTU);
            loop {
                // Each subsequent datagram: [trojanc_addr][len(2)][payload]
                let (t_addr, t_port) =
                    match protocol::read_trojanc_addr_async(&mut recv_stream).await {
                        Ok(v) => v,
                        Err(_) => break,
                    };

                let mut len_bytes = [0u8; 2];
                if recv_stream.read_exact(&mut len_bytes).await.is_err() {
                    break;
                }
                let pkt_len = u16::from_be_bytes(len_bytes) as usize;
                payload.resize(pkt_len, 0);
                if recv_stream.read_exact(&mut payload).await.is_err() {
                    break;
                }

                let target =
                    match resolve_udp_target(&t_addr, t_port, &mut domain_ip_map).await {
                        Ok(addr) => addr,
                        Err(e) => {
                            tracing::debug!("UDP target resolve error: {:?}", e);
                            break;
                        }
                    };
                if let Err(e) = remote.send_to(&payload, target).await {
                    tracing::debug!("UDP relay write error: {:?}", e);
                    break;
                }
            }
        })
    };

    let mut remote_to_quic = {
        let remote = remote.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; consts::ETHERNET_MTU];
            // Pre-allocate frame buffer for reuse across all response datagrams.
            // Max: trojanc_addr header (up to ~261 bytes) + 2-byte length + payload.
            let mut frame = Vec::with_capacity(264 + consts::ETHERNET_MTU);
            loop {
                match remote.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        // Response: [trojanc_addr][len(2)][payload] (no network byte)
                        let addr_str = addr.ip().to_string();
                        let addr_port = addr.port();
                        let hdr = match protocol::build_trojanc_addr(&addr_str, addr_port) {
                            Ok(h) => h,
                            Err(_) => break,
                        };
                        // Reuse frame buffer: clear then fill to avoid per-packet allocation.
                        let pkt_len = (n as u16).to_be_bytes();
                        frame.clear();
                        frame.extend_from_slice(&hdr);
                        frame.extend_from_slice(&pkt_len);
                        frame.extend_from_slice(&buf[..n]);
                        if send_stream.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    tokio::select! {
        _ = &mut quic_to_remote => {
            remote_to_quic.abort();
            let _ = remote_to_quic.await;
        }
        _ = &mut remote_to_quic => {
            quic_to_remote.abort();
            let _ = quic_to_remote.await;
        }
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
    // When the cache is full, evict the oldest entry (the one that will be
    // iterated first) instead of clearing the entire map. This preserves
    // recently-used entries and avoids unnecessary DNS re-resolutions.
    if domain_ip_map.len() >= consts::MAX_UDP_DNS_CACHE {
        if let Some(oldest_key) = domain_ip_map.keys().next().cloned() {
            domain_ip_map.remove(&oldest_key);
        }
    }
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
