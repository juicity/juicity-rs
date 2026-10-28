use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex as StdMutex;

use indexmap::IndexMap;
use moka::sync::Cache;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

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
    target: Arc<str>,
    cipher: Arc<UnderlayCipher>,
    /// Abort handle for the relay-back task; `None` until the task is spawned.
    relay_abort: Option<tokio::task::AbortHandle>,
}

/// Create the UDP socket shared by QUIC and underlay traffic.
fn create_server_socket(
    addr: &SocketAddr,
    dual_stack: bool,
) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if dual_stack || addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if dual_stack {
        sock.set_only_v6(false)?;
    }
    sock.bind(&(*addr).into())?;
    Ok(std::net::UdpSocket::from(sock))
}

/// Juicity proxy server.
pub struct JuicityServer {
    users: Arc<HashMap<Uuid, String>>,
    server_config: quinn::ServerConfig,
    certificate_resolver: Arc<ReloadingCertificateResolver>,
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

        let tls_builder = rustls::ServerConfig::builder().with_no_client_auth();
        let provider = tls_builder.crypto_provider().clone();
        let cert_path = config.certificate.clone();
        let key_path = config.private_key.clone();
        let certificate_resolver = Arc::new(
            tokio::task::spawn_blocking(move || {
                ReloadingCertificateResolver::new(cert_path, key_path, provider)
            })
            .await??,
        );
        let mut tls_server_config = tls_builder.with_cert_resolver(certificate_resolver.clone());

        // Juicity spec requires ALPN to be h3.
        tls_server_config.alpn_protocols = vec![b"h3".to_vec()];

        // Enable 0-RTT (Early Data), allowing the client to send early data on reconnection
        if config.enable_0rtt.unwrap_or(true) {
            tls_server_config.max_early_data_size = u32::MAX;
        }

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls_server_config)?,
        ));

        let mut transport_config = quinn::TransportConfig::default();

        // Set initial_rtt if configured
        if let Some(initial_rtt_ms) = config.initial_rtt {
            transport_config.initial_rtt(std::time::Duration::from_millis(initial_rtt_ms));
        }

        // Keep-alive is disabled by default so QUIC idle timeout / connection
        // lifecycle management can release idle connections naturally.
        // Enable only when explicitly configured.
        if let Some(keep_alive_secs) = config.keep_alive_interval {
            transport_config
                .keep_alive_interval(Some(std::time::Duration::from_secs(keep_alive_secs)));
        }

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
        if let Some(rtt_ms) = config.initial_rtt {
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

        match config.congestion_control.to_lowercase().as_str() {
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
            certificate_resolver,
            dialer,
            in_flight: Arc::new(crate::inflight::InFlightUnderlayKey::new(
                consts::IN_FLIGHT_UNDERLAY_TTL,
                config
                    .underlay_evict_timeout
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(consts::IN_FLIGHT_UNDERLAY_EVICT_TIMEOUT),
            )),
            udp_endpoint_pool: Arc::new(crate::udp::UdpEndpointPool::new(
                consts::MAX_UDP_ENDPOINTS as u64,
            )),
            disable_outbound_udp443: config.disable_outbound_udp443,
        })
    }

    pub async fn serve(&self, addr: &str) -> anyhow::Result<()> {
        self.serve_with_shutdown(addr, std::future::pending::<()>())
            .await
    }

    /// Serve until an external shutdown signal resolves.
    ///
    /// Shutdown sequence:
    /// 1) signal accept loops to stop taking new connections
    /// 2) allow a grace period for in-flight connection handlers to drain
    /// 3) abort remaining accept workers/tasks that did not exit in time
    pub async fn serve_with_shutdown<F>(&self, addr: &str, shutdown: F) -> anyhow::Result<()>
    where
        F: Future<Output = ()> + Send,
    {
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

        // A single endpoint owns every connection ID, including after NAT rebinding.
        // Independent SO_REUSEPORT endpoints cannot route migrated connections.
        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow::anyhow!("no async runtime found for quinn"))?;
        let (underlay_tx, underlay_rx) =
            tokio::sync::mpsc::channel(crate::underlay_socket::UNDERLAY_CHANNEL_CAPACITY);
        let socket = create_server_socket(&socket_addr, dual_stack)?;
        socket.set_nonblocking(true)?;
        let server_underlay_socket =
            Arc::new(tokio::net::UdpSocket::from_std(socket.try_clone()?)?);
        let demux = Arc::new(crate::underlay_socket::DemuxUdpSocket::new(
            runtime.wrap_udp_socket(socket)?,
            underlay_tx,
        ));
        // The demultiplexer reserves packets with a clear QUIC fixed bit for underlay.
        let mut endpoint_config = EndpointConfig::default();
        endpoint_config.grease_quic_bit(false);
        let endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config,
            Some(self.server_config.clone()),
            demux,
            runtime,
        )?;
        tracing::info!("Juicity server listening on {}", log_addr);

        let certificate_resolver = self.certificate_resolver.clone();
        let _certificate_guard = AbortOnDrop(
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let resolver = certificate_resolver.clone();
                    match tokio::task::spawn_blocking(move || resolver.reload()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "TLS certificate reload failed; keeping previous certificate"),
                        Err(error) => tracing::warn!(%error, "TLS certificate reload task failed"),
                    }
                }
            })
            .abort_handle(),
        );

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

        let underlay_in_flight = self.in_flight.clone();
        let underlay_udp_pool = self.udp_endpoint_pool.clone();
        let underlay_disable_443 = self.disable_outbound_udp443;
        let underlay_socket = server_underlay_socket.clone();
        // The underlay loop self-terminates when underlay_rx closes (all endpoints dropped),
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

        // Signal shutdown to the accept worker.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        // If an accept worker does not stop promptly after shutdown is broadcast,
        // abort it after this timeout.
        let accept_stop_timeout = std::time::Duration::from_secs(3);
        // Grace period for connection handlers after accepting stops.
        let connection_drain_timeout = std::time::Duration::from_secs(5);
        // Bound concurrent connection handlers.
        let max_concurrent_connection_handlers = 1024usize;

        let users = self.users.clone();
        let in_flight = self.in_flight.clone();
        let udp_pool = self.udp_endpoint_pool.clone();
        let dialer = self.dialer.clone();
        let disable_443 = self.disable_outbound_udp443;
        let mut accept_shutdown = shutdown_rx;

        let accept_handle = tokio::spawn(async move {
            let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            let conn_limit = Arc::new(tokio::sync::Semaphore::new(
                max_concurrent_connection_handlers,
            ));

            loop {
                while conn_tasks.try_join_next().is_some() {}

                tokio::select! {
                    changed = accept_shutdown.changed() => {
                        match changed {
                            Ok(()) if *accept_shutdown.borrow() => {
                                break;
                            }
                            Ok(()) => {}
                            Err(_) => {
                                break;
                            }
                        }
                    }
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else {
                            break;
                        };

                        let permit = match conn_limit.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => break,
                        };

                        let users = users.clone();
                        let in_flight = in_flight.clone();
                        let udp_pool = udp_pool.clone();
                        let dialer = dialer.clone();
                        let disable_443 = disable_443;

                        conn_tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = handle_connection(
                                incoming,
                                users,
                                in_flight,
                                udp_pool,
                                dialer,
                                disable_443,
                            )
                            .await
                            {
                                tracing::warn!("Connection handler error: {:?}", e);
                            }
                        });
                    }
                }
            }

            // Drain existing connection handlers first.
            let deadline = tokio::time::Instant::now() + connection_drain_timeout;
            while !conn_tasks.is_empty() {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, conn_tasks.join_next()).await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }

            // Any still-running handlers are force-aborted after grace period.
            if !conn_tasks.is_empty() {
                conn_tasks.abort_all();
                while conn_tasks.join_next().await.is_some() {}
            }
        });

        let accept_guard = AbortOnDrop(accept_handle.abort_handle());

        // Wait until shutdown is requested by the caller.
        shutdown.await;
        tracing::info!("shutdown requested: stopping accept loop");

        // 1) Stop accepting new connections.
        let _ = shutdown_tx.send(true);

        // 2) Let the accept worker drain in-flight connection handlers.
        let stop_deadline = tokio::time::Instant::now() + accept_stop_timeout;
        while tokio::time::Instant::now() < stop_deadline {
            if accept_handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 3) Force-abort the accept worker if it has not exited.
        drop(accept_guard);

        // Join the worker so task resources are reclaimed before returning.
        let _ = accept_handle.await;

        Ok(())
    }
}

/// Background task loop that processes non-QUIC underlay packets received
/// from the demultiplexed channel.
///
/// This function is spawned as a long-running async task inside
/// [`JuicityServer::serve`](JuicityServer::serve).  It reads
/// [`UnderlayPacket`]s from the channel, applies a concurrency limit via a
/// [`tokio::sync::Semaphore`], and spawns individual handler tasks via
/// [`handle_non_quic_underlay_packet`].
///
/// It also manages the underlay session map (a
/// [`moka::sync::Cache`] of [`UnderlaySession`]) which handles
/// automatic TTL-based eviction of idle sessions and LRU eviction
/// when at capacity.
///
/// # Arguments
///
/// * `rx` - Receiver end of the underlay channel, fed by
///   [`DemuxUdpSocket`](crate::underlay_socket::DemuxUdpSocket).
/// * `in_flight` - Shared in-flight underlay auth table used for salt-based
///   authentication of new sessions.
/// * `udp_pool` - Shared UDP endpoint pool for full-cone NAT.
/// * `server_socket` - The server's main UDP socket, used for relay-back
///   traffic to clients.
/// * `disable_udp_443` - When `true`, outbound UDP to port 443 is blocked.
///
/// # Lifespan
///
/// The loop exits when the channel `rx` is closed (i.e. the [`quinn::Endpoint`]
/// is dropped), at which point all spawned subtasks are cancelled via
/// [`AbortOnDrop`] guards.
async fn run_underlay_packet_loop(
    mut rx: tokio::sync::mpsc::Receiver<crate::underlay_socket::UnderlayPacket>,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    server_socket: Arc<tokio::net::UdpSocket>,
    disable_udp_443: bool,
) {
    // ══ Moka cache for underlay sessions ══════════════════════════════════
    // Uses moka::sync::Cache with:
    //   - time_to_idle: entries idle for > DEFAULT_NAT_TIMEOUT are auto-evicted
    //   - max_capacity: LRU eviction when at MAX_UNDERLAY_SESSIONS
    //   - eviction_listener: aborts relay-back tasks and removes from UDP pool
    //
    // This replaces the manual DashMap cleanup loop and capacity O(n) scan.
    let sessions: Arc<Cache<SocketAddr, UnderlaySession>> = {
        let udp_pool_for_evict = udp_pool.clone();
        Arc::new(
            Cache::builder()
                .max_capacity(consts::MAX_UNDERLAY_SESSIONS as u64)
                .time_to_idle(consts::DEFAULT_NAT_TIMEOUT)
                .eviction_listener(move |_k: Arc<SocketAddr>, _v: UnderlaySession, _cause| {
                    if let Some(h) = _v.relay_abort {
                        h.abort();
                    }
                    udp_pool_for_evict.remove(&*_k);
                })
                .build(),
        )
    };

    // Limit concurrent underlay packet handler tasks to prevent unbounded task
    // accumulation under high traffic. Each handler may wait up to 100ms in
    // evict() for in-flight underlay auth, and without a cap, thousands of
    // tasks could pile up during a burst, consuming significant memory.
    // Use a fixed cap to keep memory predictable under burst traffic.
    let concurrency_limit = Arc::new(tokio::sync::Semaphore::new(
        consts::MAX_UNDERLAY_HANDLER_CONCURRENCY,
    ));

    while let Some(packet) = rx.recv().await {
        if packet.payload.len() < consts::UNDERLAY_SALT_LEN {
            continue;
        }

        let source = packet.peer;

        // ── Fast path: existing session → decrypt + forward inline (no spawn) ──
        // Only attempt inline processing when both session and endpoint are cached.
        // This avoids modifying payload in-place when we might fall through to the
        // slow path (which expects the original salt-prefixed payload).
        if let Some(session) = sessions.get(&source) {
            if let Some((send_socket, pool_target)) = udp_pool.get_socket(&source) {
                // Clone payload only when we're committed to the fast path.
                // This is the hot path — most packets hit here.
                let mut payload = packet.payload.clone();
                if session.cipher.decrypt_in_place(&mut payload).is_ok() {
                    if crate::udp::send_to(
                        &send_socket,
                        &payload[juicity_underlay::SALT_LEN..],
                        &*pool_target,
                    )
                    .await
                    .is_ok()
                    {
                        continue;
                    }
                    // send failed — clean up stale endpoint, fall through to slow path
                    udp_pool.remove(&source);
                }
                // decrypt or send failed — fall through to slow path
            }
        }

        // ── Slow path: new session or recovery → spawn task for async auth/endpoint creation ──
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

/// Process a single non-QUIC underlay packet from a client.
///
/// This is the core handler for all non-QUIC UDP traffic received on the
/// shared server port.  It performs the following steps:
///
/// 1. **Existing session fast path** — If the source address already has a
///    cached [`UnderlaySession`], decrypt the payload in-place using the
///    session's cipher and forward it to the cached target via the UDP
///    endpoint pool.
/// 2. **New session slow path** — Otherwise, extract the salt from the
///    payload, look up the corresponding in-flight underlay auth (via
///    [`InFlightUnderlayKey::evict`]), derive a per-session cipher,
///    create a UDP endpoint (+ relay-back task), and insert the session
///    into the LRU cache.
///
/// # Arguments
///
/// * `packet` - The incoming non-QUIC underlay packet, containing the
///   source address and raw payload.
/// * `in_flight` - The shared in-flight auth table used to match salts to
///   authenticated underlay sessions.
/// * `udp_pool` - The shared UDP endpoint pool for full-cone NAT.
/// * `sessions` - The shared [`moka::sync::Cache`] of active [`UnderlaySession`]s.
/// * `server_socket` - The server's main UDP socket, used for relay-back
///   traffic.
/// * `disable_udp_443` - When `true`, outbound UDP to port 443 is blocked.
///
/// # Returns
///
/// * `Ok(())` — The packet was processed (or silently dropped due to
///   invalid auth / decryption failure).
/// * `Err(anyhow::Error)` — A fatal error occurred (e.g. DNS failure,
///   socket creation failure).
///
/// # Errors
///
/// Non-fatal errors (decryption failure, missing auth, invalid packet
/// length) are logged at `debug` level and return `Ok(())`, so a single
/// malformed packet does not disrupt the overall handler loop.
async fn handle_non_quic_underlay_packet(
    packet: crate::underlay_socket::UnderlayPacket,
    in_flight: Arc<crate::inflight::InFlightUnderlayKey>,
    udp_pool: Arc<crate::udp::UdpEndpointPool>,
    sessions: Arc<Cache<SocketAddr, UnderlaySession>>,
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
        match sessions.get(&source) {
            Some(s) => {
                // moka's time_to_idle handles TTL automatically.
                // A successful get() means the entry is still valid.
                // Extract only the fields we need:
                // - cipher: Arc clone is just a refcount increment
                // - target: Arc<str> clone is just a refcount increment
                Some((s.cipher.clone(), s.target.clone()))
            }
            None => None,
        }
    };

    // ── Existing session: decrypt + forward immediately ──
    if let Some((ref cipher, ref target)) = existing_session {
        // In-place decrypt using cached cipher (plaintext at &payload[SALT_LEN..])
        if let Err(e) = cipher.decrypt_in_place(&mut payload) {
            tracing::debug!(
                "drop invalid underlay packet from {} for target {}: {:?}",
                source,
                &**target,
                e
            );
            return Ok(());
        }

        // Fast path: pool returns tokio::net::UdpSocket directly —
        // no expensive from_std() syscall registration needed.
        let (send_socket, pool_target) = match udp_pool.get_socket(&source) {
            Some(result) => result,
            None => {
                // Pool endpoint expired — create a new one (rare).
                udp_pool
                    .get_or_create(
                        source,
                        crate::udp::UdpEndpointOptions {
                            nat_timeout: consts::DEFAULT_NAT_TIMEOUT,
                            dial_target: String::from(target.as_ref()),
                        },
                    )
                    .await?
                    .0
            }
        };
        if let Err(e) = crate::udp::send_to(
            &send_socket,
            &payload[juicity_underlay::SALT_LEN..],
            &*pool_target,
        )
        .await
        {
            udp_pool.remove(&source);
            // Remove the session and abort its relay task.
            if let Some(s) = sessions.get(&source) {
                if let Some(h) = s.relay_abort {
                    h.abort();
                }
            }
            sessions.invalidate(&source);
            return Err(anyhow::anyhow!(
                "underlay send_to {} failed: {:?}",
                &*pool_target,
                e
            ));
        }
        return Ok(());
    }

    // ── New session: auth, decrypt, create endpoint+relay, then insert ──
    let mut salt = [0u8; consts::UNDERLAY_SALT_LEN];
    salt.copy_from_slice(&payload[..consts::UNDERLAY_SALT_LEN]);
    let (auth, cipher) = match in_flight
        .evict(&salt, |auth| {
            // Keep auth available until a packet proves possession of its PSK.
            let subkey = juicity_underlay::derive_subkey(&auth.psk, &salt)
                .expect("derive_subkey failed: invalid PSK length");
            let cipher = UnderlayCipher::from_subkey(&subkey);
            if let Err(e) = cipher.decrypt_in_place(&mut payload) {
                tracing::debug!(
                    "drop first underlay packet from {} for target {}: {:?}",
                    source,
                    auth.metadata.target_addr(),
                    e
                );
                return None;
            }
            Some(cipher)
        })
        .await
    {
        Some(verified) => verified,
        None => {
            tracing::debug!(
                "drop non-QUIC packet from {}: missing or invalid in-flight underlay auth",
                source
            );
            return Ok(());
        }
    };

    if disable_udp_443 && auth.metadata.port == 443 {
        tracing::debug!("blocked underlay UDP/443: {}", auth.metadata.target_addr());
        return Ok(());
    }

    let cipher = Arc::new(cipher);
    let target = auth.metadata.target_addr();

    // ── Create UDP endpoint and spawn relay-back task BEFORE inserting session ──
    // This eliminates the race window where the cleanup task could remove the session
    // between insertion and abort handle storage, which would orphan the relay task.
    let ((udp_socket, dial_target), is_new) = udp_pool
        .get_or_create(
            source,
            crate::udp::UdpEndpointOptions {
                nat_timeout: consts::DEFAULT_NAT_TIMEOUT,
                dial_target: target.clone(),
            },
        )
        .await?;

    // Pool returns tokio socket directly — no from_std() needed.
    // Convert BEFORE any spawn so a conversion failure
    // (e.g. kernel fd exhaustion) cannot produce an orphaned relay-back task.
    let recv_socket_for_relay = if is_new {
        Some(udp_socket.clone())
    } else {
        None
    };
    let send_socket = udp_socket;

    let relay_abort = if let Some(recv_socket) = recv_socket_for_relay {
        let relay_back = server_socket.clone();
        let session_cipher = cipher.clone();
        let sessions_for_task = sessions.clone();
        let udp_pool_for_task = udp_pool.clone();

        // ── SessionGuard ──────────────────────────────────────────────────
        // A Drop guard that ensures the session entry is invalidated from the
        // sessions cache when the relay-back task exits, *even if the task is
        // externally aborted via AbortHandle*.
        //
        // When abort() is called on a running task, tokio drops the future
        // at the next await point — local variables go through Drop, but code
        // after the loop (the manual cleanup below) never executes.
        // SessionGuard's Drop impl runs regardless of how the task terminates:
        //   - Normal exit (loop breaks) → guard is dropped → session invalidated.
        //   - External abort             → guard is dropped → session invalidated.
        //   - Panic                      → guard is dropped → session invalidated.
        struct SessionGuard {
            source: SocketAddr,
            sessions: Arc<Cache<SocketAddr, UnderlaySession>>,
        }
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                self.sessions.invalidate(&self.source);
            }
        }

        let _guard = SessionGuard {
            source,
            sessions: sessions_for_task.clone(),
        };

        let relay_handle = tokio::spawn(async move {
            // Pre-allocate full-capacity output buffer to avoid repeated Vec resizing.
            // Max payload: ETHERNET_MTU * 4, plus 32-byte salt prefix + 16-byte AEAD tag.
            let max_out_len = consts::ETHERNET_MTU * 4 + 48;
            let mut buf = vec![0u8; consts::ETHERNET_MTU * 4];
            let mut outbuf = Vec::with_capacity(max_out_len);

            // ── Pre-generated salt pool ──
            // Generates 4096 salts upfront (single fill_bytes call) to amortise
            // CSPRNG overhead.  When the pool is exhausted, a fresh batch is
            // generated.  4096 × 32 bytes = 128 KB per task, acceptable for the
            // 180-second NAT timeout window.
            let mut salt_pool = generate_salt_batch();

            loop {
                match tokio::time::timeout(
                    consts::DEFAULT_NAT_TIMEOUT,
                    recv_socket.recv_from(&mut buf),
                )
                .await
                {
                    Ok(Ok((n, _))) => {
                        // Get a salt from the pre-generated pool; refill if empty.
                        let salt = salt_pool.pop().unwrap_or_else(|| {
                            let new_batch = generate_salt_batch();
                            salt_pool = new_batch;
                            salt_pool.pop().unwrap()
                        });
                        // Pre-allocate with SALT_LEN headroom at front — avoids O(n) shift in encrypt_in_place
                        outbuf.clear();
                        outbuf.reserve(n + juicity_underlay::SALT_LEN + juicity_underlay::TAG_LEN);
                        outbuf.resize(juicity_underlay::SALT_LEN, 0);
                        outbuf.extend_from_slice(&buf[..n]);
                        if session_cipher.encrypt_in_place(&mut outbuf, &salt).is_err() {
                            break;
                        }

                        if crate::udp::send_to(&relay_back, &outbuf, source)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!("underlay endpoint recv failed for {}: {:?}", source, e);
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }

            // Drop the SessionGuard explicitly before the pool remove so the
            // session map is cleaned up synchronously.  The guard is otherwise
            // dropped by the compiler at the end of the scope, which is fine too.
            drop(_guard);

            // Pool cleanup uses std::sync::Mutex (non-async) so the
            // periodic pool cleanup (every 10s) will handle stale entries.
            udp_pool_for_task.remove(&source);
        });
        Some(relay_handle.abort_handle())
    } else {
        None
    };

    let session = UnderlaySession {
        target: Arc::from(target.as_str()),
        cipher,
        relay_abort,
    };

    // Insert session — moka's max_capacity handles LRU eviction automatically.
    // The eviction listener aborts the relay task and removes from the UDP pool.
    sessions.insert(source, session.clone());
    tracing::debug!("new underlay session {} -> {}", source, &*session.target);

    // Send first packet immediately — relay task is already running.
    // Plaintext is at &payload[SALT_LEN..] (salt prefix kept in place).
    if let Err(e) = crate::udp::send_to(
        &send_socket,
        &payload[juicity_underlay::SALT_LEN..],
        &*dial_target,
    )
    .await
    {
        udp_pool.remove(&source);
        if let Some(s) = sessions.get(&source) {
            if let Some(h) = s.relay_abort {
                h.abort();
            }
        }
        sessions.invalidate(&source);
        return Err(anyhow::anyhow!(
            "underlay send_to {} failed: {:?}",
            dial_target,
            e
        ));
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
    tracing::info!(
        remote_addr = %remote_addr,
        event = "new_connection",
        "New QUIC connection"
    );

    // === Authenticate ===
    let auth_conn = connection.clone();
    let auth_users = users.clone();

    let auth_result = tokio::time::timeout(consts::AUTHENTICATE_TIMEOUT, async {
        handle_auth(&auth_conn, auth_users).await
    })
    .await;

    let (user_uuid, mut auth_uni_stream) = match auth_result {
        Ok(Ok((uuid, stream))) => {
            tracing::info!(
                user = %uuid,
                remote_addr = %remote_addr,
                event = "authentication",
                "User authenticated"
            );
            (uuid, stream)
        }
        Ok(Err(e)) => {
            tracing::warn!(
                remote_addr = %remote_addr,
                error = %e,
                event = "authentication_failed",
                "Authentication failed"
            );
            connection.close(VarInt::from_u32(0xfffffff1), b"authentication failed");
            return Err(e);
        }
        Err(_) => {
            connection.close(VarInt::from_u32(0xfffffff2), b"authentication timeout");
            return Err(anyhow::anyhow!("auth timeout"));
        }
    };

    // Shared DNS cache across all UDP relay streams within this QUIC connection.
    // Uses StdMutex because critical sections are sub-microsecond with no .await
    // held under lock — avoids the heavier tokio::sync::Mutex waker overhead.
    let dns_cache: Arc<StdMutex<IndexMap<(Arc<str>, u16), (SocketAddr, Instant)>>> =
        Arc::new(StdMutex::new(IndexMap::new()));
    // Tracks in-flight DNS resolutions so concurrent lookups for the same
    // domain don't all issue DNS queries in parallel.
    let dns_resolving: Arc<StdMutex<HashMap<(Arc<str>, u16), Arc<tokio::sync::Notify>>>> =
        Arc::new(StdMutex::new(HashMap::new()));

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
                    tracing::debug!("Underlay auth stream closed for {}: {:?}", user_uuid, e);
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
                let s_dns_cache = dns_cache.clone();
                let s_dns_resolving = dns_resolving.clone();

                stream_tasks.spawn(async move {
                    if let Err(e) = handle_stream(
                        send_stream,
                        recv_stream,
                        s_dialer,
                        s_user_uuid,
                        s_disable_443,
                        s_dns_cache,
                        s_dns_resolving,
                    )
                    .await
                    {
                        tracing::debug!("Stream handler error: {:?}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                // QUIC connection closed normally by peer
                tracing::info!(
                    remote_addr = %remote_addr,
                    event = "connection_closed",
                    "Connection closed by peer"
                );
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

    // Verify token using TLS ExportKeyingMaterial (RFC 5705) - same as upstream.
    // export_keying_material is CPU-bound (HKDF); run it in spawn_blocking to
    // avoid occupying the async event loop during connection bursts.
    let conn_for_token = conn.clone();
    let uuid_for_token = uuid;
    let password_for_token = password.clone();
    let expected_token = tokio::task::spawn_blocking(move || {
        protocol::gen_token_via_connection(&conn_for_token, &uuid_for_token, &password_for_token)
    })
    .await??;

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
    dns_cache: Arc<StdMutex<IndexMap<(Arc<str>, u16), (SocketAddr, Instant)>>>,
    dns_resolving: Arc<StdMutex<HashMap<(Arc<str>, u16), Arc<tokio::sync::Notify>>>>,
) -> anyhow::Result<()> {
    // Read proxy header via async reader
    let (network, hostname, port) = protocol::read_proxy_header_async(&mut recv_stream).await?;
    let target = match hostname.parse::<std::net::IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{}:{}", hostname, port),
    };

    match network {
        protocol::NETWORK_TCP => {
            tracing::info!(
                user = %user_uuid,
                target = %target,
                protocol = "tcp",
                event = "relay",
                "TCP relay"
            );
            handle_tcp_relay(send_stream, recv_stream, dialer, &target).await
        }
        protocol::NETWORK_UDP => {
            tracing::info!(
                user = %user_uuid,
                protocol = "udp",
                event = "relay",
                "UDP relay"
            );
            handle_udp_relay(
                send_stream,
                recv_stream,
                dialer,
                disable_udp_443,
                dns_cache,
                dns_resolving,
            )
            .await
        }
        _ => anyhow::bail!("unknown network type: {}", network),
    }
}

/// Relay TCP while preserving half-close and sharing one idle deadline.
async fn handle_tcp_relay(
    send_stream: SendStream,
    recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    target: &str,
) -> anyhow::Result<()> {
    let remote = dialer.dial_tcp(target).await?;
    crate::tcp::relay(
        recv_stream,
        send_stream,
        remote,
        consts::TCP_RELAY_IDLE_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// UDP over Stream relay.
/// UDP over Stream relay — upstream-compatible wire format.
///
/// Each UDP datagram on the stream (both directions):
///   [trojanc_addr][len(2)][payload]
/// No network byte per datagram; the stream header already carries it.
async fn handle_udp_relay(
    send_stream: SendStream,
    mut recv_stream: RecvStream,
    dialer: Arc<dyn crate::dialer::Dialer>,
    disable_udp_443: bool,
    dns_cache: Arc<StdMutex<IndexMap<(Arc<str>, u16), (SocketAddr, Instant)>>>,
    dns_resolving: Arc<StdMutex<HashMap<(Arc<str>, u16), Arc<tokio::sync::Notify>>>>,
) -> anyhow::Result<()> {
    // First datagram: [trojanc_addr][len(2)][payload]
    let (first_host, first_port) = protocol::read_trojanc_addr_async(&mut recv_stream).await?;

    if disable_udp_443 && first_port == 443 {
        tracing::debug!("Blocked UDP/443: {}:{}", first_host, first_port);
        return Ok(());
    }

    let first_target_addr =
        resolve_udp_target(&first_host, first_port, &dns_cache, &dns_resolving).await?;
    let remote = dialer.dial_udp(&first_target_addr.to_string()).await?;
    let remote = Arc::new(remote);

    let mut len_buf = [0u8; 2];
    recv_stream.read_exact(&mut len_buf).await?;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    // Allocate once with ETHERNET_MTU capacity; the same buffer is reused
    // for the first datagram here and then passed into quic_to_remote to
    // serve as the per-packet buffer — eliminating an extra heap allocation.
    let mut data = Vec::with_capacity(consts::ETHERNET_MTU);
    data.resize(pkt_len, 0);
    recv_stream.read_exact(&mut data).await?;
    crate::udp::send_to(&remote, &data, first_target_addr).await?;

    // Bidirectional relay
    let mut quic_to_remote = {
        let remote = remote.clone();
        let dns_cache = dns_cache.clone();
        let dns_resolving = dns_resolving.clone();
        tokio::spawn(async move {
            // Reuse the first-datagram buffer for all subsequent datagrams.
            let mut payload = data;
            // Reusable string buffer for per-datagram address — avoids a String
            // heap allocation on every UDP datagram in the hot loop.
            let mut t_addr_buf = String::with_capacity(64);
            // Local resolved-address cache: most UDP sessions talk to the same
            // target, so this avoids repeated `Arc<str>` allocation + DNS lookup.
            let mut local_resolved: std::collections::HashMap<(String, u16), SocketAddr> =
                std::collections::HashMap::new();
            loop {
                // Each subsequent datagram: [trojanc_addr][len(2)][payload]
                let t_port =
                    match protocol::read_trojanc_addr_into_async(&mut recv_stream, &mut t_addr_buf)
                        .await
                    {
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

                // Fast local cache: avoid DNS lookup + Arc<str> allocation for
                // the common case where all datagrams go to the same target.
                let target = if let Some(&cached) = local_resolved.get(&(t_addr_buf.clone(), t_port)) {
                    cached
                } else {
                    let resolved = match resolve_udp_target(
                        &t_addr_buf,
                        t_port,
                        &dns_cache,
                        &dns_resolving,
                    )
                    .await
                    {
                        Ok(addr) => addr,
                        Err(e) => {
                            tracing::debug!("UDP target resolve error: {:?}", e);
                            break;
                        }
                    };
                    local_resolved.insert((t_addr_buf.clone(), t_port), resolved);
                    resolved
                };
                if let Err(e) = crate::udp::send_to(&remote, &payload, target).await {
                    tracing::debug!("UDP relay write error: {:?}", e);
                    break;
                }
            }
        })
    };

    let mut remote_to_quic = {
        let remote = remote.clone();
        tokio::spawn(async move {
            // RAII guard: ensure send_stream.finish() is called even when this
            // task is aborted.  Without this, the QUIC send stream would be
            // left in a half-closed state until the connection idle timeout
            // fires, holding stream resources unnecessarily.
            struct SendGuard {
                send: Option<SendStream>,
            }
            impl Drop for SendGuard {
                fn drop(&mut self) {
                    if let Some(ref mut s) = self.send {
                        let _ = s.finish();
                    }
                }
            }
            let mut guard = SendGuard {
                send: Some(send_stream),
            };
            if let Err(e) = crate::udp::relay_responses(
                remote.as_ref(),
                guard.send.as_mut().unwrap(),
            )
            .await
            {
                tracing::debug!("UDP relay remote->quic error: {:?}", e);
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

/// Resolve a UDP target with a bounded TTL cache and per-key single-flight.
///
/// Failed or cancelled lookups release the key and wake all waiters. The
/// entire lookup, including time spent waiting, is bounded by DNS_QUERY_TIMEOUT.
async fn resolve_udp_target(
    host: &str,
    port: u16,
    dns_cache: &StdMutex<IndexMap<(Arc<str>, u16), (SocketAddr, Instant)>>,
    dns_resolving: &StdMutex<HashMap<(Arc<str>, u16), Arc<tokio::sync::Notify>>>,
) -> anyhow::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    struct ResolutionGuard<'a> {
        key: (Arc<str>, u16),
        resolving: &'a StdMutex<HashMap<(Arc<str>, u16), Arc<tokio::sync::Notify>>>,
        notify: Arc<tokio::sync::Notify>,
    }

    impl Drop for ResolutionGuard<'_> {
        fn drop(&mut self) {
            self.resolving
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.key);
            self.notify.notify_waiters();
        }
    }

    let key = (Arc::from(host), port);
    let deadline = tokio::time::Instant::now() + consts::DNS_QUERY_TIMEOUT;
    loop {
        let resolution = {
            let mut resolving = dns_resolving.lock().unwrap_or_else(|e| e.into_inner());
            {
                let mut cache = dns_cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((mapped, timestamp)) = cache.get(&key) {
                    if timestamp.elapsed() < consts::UDP_DNS_CACHE_TTL {
                        return Ok(*mapped);
                    }
                    cache.swap_remove(&key);
                }
            }

            if let Some(notify) = resolving.get(&key) {
                // Capture notify_waiters' generation before releasing the lock.
                Err(notify.clone().notified_owned())
            } else {
                let notify = Arc::new(tokio::sync::Notify::new());
                resolving.insert(key.clone(), notify.clone());
                Ok(ResolutionGuard {
                    key: key.clone(),
                    resolving: dns_resolving,
                    notify,
                })
            }
        };
        let _guard = match resolution {
            Ok(guard) => guard,
            Err(wait) => {
                tokio::time::timeout_at(deadline, wait)
                    .await
                    .map_err(|_| anyhow::anyhow!("DNS query timeout for {}:{}", host, port))?;
                continue;
            }
        };

        let resolved = tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| anyhow::anyhow!("DNS query timeout for {}:{}", host, port))??
            .next()
            .ok_or_else(|| anyhow::anyhow!("no DNS result for {}:{}", host, port))?;

        let mut cache = dns_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= consts::MAX_UDP_DNS_CACHE {
            if let Some(oldest_key) = cache.keys().next().cloned() {
                cache.swap_remove(&oldest_key);
            }
        }
        cache.insert(key.clone(), (resolved, Instant::now()));
        return Ok(resolved);
    }
}

/// Pre-generate a batch of underlay salts using a single `fill_bytes` call
/// to amortise CSPRNG overhead.  Each salt has bytes [0..=1] = 0 and
/// bytes [2..32] filled with random data (matching `generate_underlay_salt`).
fn generate_salt_batch() -> Vec<[u8; 32]> {
    const BATCH_SIZE: usize = 4096;
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    // Generate all random bytes in one shot (30 random bytes × 4096 = 122880).
    use rand::RngCore;
    let mut random_bytes = vec![0u8; BATCH_SIZE * 30];
    rand::thread_rng().fill_bytes(&mut random_bytes);

    for i in 0..BATCH_SIZE {
        let mut salt = [0u8; 32];
        salt[0] = 0;
        salt[1] = 0;
        let offset = i * 30;
        salt[2..].copy_from_slice(&random_bytes[offset..offset + 30]);
        batch.push(salt);
    }
    batch
}

#[derive(Debug)]
struct ReloadingCertificateResolver {
    cert_path: String,
    key_path: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
    cached: std::sync::RwLock<CachedCertificate>,
}

#[derive(Debug)]
struct CachedCertificate {
    key: Arc<rustls::sign::CertifiedKey>,
    modified: (std::time::SystemTime, std::time::SystemTime),
}

impl ReloadingCertificateResolver {
    fn new(
        cert_path: String,
        key_path: String,
        provider: Arc<rustls::crypto::CryptoProvider>,
    ) -> anyhow::Result<Self> {
        let modified = Self::modified(&cert_path, &key_path)?;
        let key = rustls::sign::CertifiedKey::from_der(
            load_certs(&cert_path)?,
            load_private_key(&key_path)?,
            &provider,
        )?;
        Ok(Self {
            cert_path,
            key_path,
            provider,
            cached: std::sync::RwLock::new(CachedCertificate {
                key: Arc::new(key),
                modified,
            }),
        })
    }

    fn modified(
        cert_path: &str,
        key_path: &str,
    ) -> std::io::Result<(std::time::SystemTime, std::time::SystemTime)> {
        Ok((
            std::fs::metadata(cert_path)?.modified()?,
            std::fs::metadata(key_path)?.modified()?,
        ))
    }

    fn reload(&self) -> anyhow::Result<()> {
        let modified = Self::modified(&self.cert_path, &self.key_path)?;
        if self
            .cached
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .modified
            == modified
        {
            return Ok(());
        }
        // Publish the pair only after parsing and key matching succeed. A renewal
        // may replace the files separately; failed loads must be retried.
        let key = rustls::sign::CertifiedKey::from_der(
            load_certs(&self.cert_path)?,
            load_private_key(&self.key_path)?,
            &self.provider,
        )?;
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = CachedCertificate {
            key: Arc::new(key),
            modified,
        };
        tracing::info!("TLS certificate reloaded");
        Ok(())
    }
}

impl rustls::server::ResolvesServerCert for ReloadingCertificateResolver {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        // Handshakes only read the cache; file I/O runs on the blocking pool.
        Some(
            self.cached
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .key
                .clone(),
        )
    }
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

#[cfg(test)]
mod tests;
