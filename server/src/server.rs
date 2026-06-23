use std::collections::HashMap;
use std::num::NonZeroUsize;

use indexmap::IndexMap;
use lru::LruCache;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
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
    target: String,
    cipher: Arc<UnderlayCipher>,
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

        // Set keep_alive_interval if configured; otherwise use default
        let keep_alive = config
            .keep_alive_interval
            .map(std::time::Duration::from_secs)
            .unwrap_or(consts::KEEP_ALIVE_PERIOD);
        transport_config.keep_alive_interval(Some(keep_alive));

        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        transport_config.max_concurrent_uni_streams(VarInt::from_u32(
            consts::MAX_OPEN_INCOMING_STREAMS as u32,
        ));
        // Set an explicit idle timeout for defense-in-depth.
        // Even with keep-alive enabled, if the peer stops responding or never opens
        // a stream after authentication, this timeout ensures the connection and its
        // associated resources (auth reader task, Arc references) are eventually released.
        transport_config.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(consts::MAX_QUIC_IDLE_TIMEOUT)
                .map_err(|e| anyhow::anyhow!("invalid idle timeout: {:?}", e))?,
        ));
        transport_config.stream_receive_window(VarInt::from_u32(
            consts::QUIC_STREAM_RECEIVE_WINDOW,
        ));
        transport_config.receive_window(VarInt::from_u32(
            consts::QUIC_CONNECTION_RECEIVE_WINDOW,
        ));
        transport_config.send_window(consts::QUIC_SEND_WINDOW);

        // Dynamically adjust window size based on initial_rtt
        if let Some(rtt_ms) = config.initial_rtt {
            if rtt_ms < 50 {
                // Low latency: reduce window to save memory
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW / 2,
                ));
                transport_config.receive_window(VarInt::from_u32(
                    consts::QUIC_CONNECTION_RECEIVE_WINDOW / 2,
                ));
            } else if rtt_ms > 200 {
                // High latency: increase window to improve throughput
                transport_config.stream_receive_window(VarInt::from_u32(
                    consts::QUIC_STREAM_RECEIVE_WINDOW * 2,
                ));
                transport_config.receive_window(VarInt::from_u32(
                    consts::QUIC_CONNECTION_RECEIVE_WINDOW * 2,
                ));
            }
        }

        match config.congestion_control.to_lowercase().as_str() {
            "cubic" => transport_config.congestion_controller_factory(
                Arc::new(quinn::congestion::CubicConfig::default()),
            ),
            "newreno" | "new_reno" => transport_config.congestion_controller_factory(
                Arc::new(quinn::congestion::NewRenoConfig::default()),
            ),
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
            dialer,
            in_flight: Arc::new(crate::inflight::InFlightUnderlayKey::new(
                consts::IN_FLIGHT_UNDERLAY_TTL,
                config
                    .underlay_evict_timeout
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(consts::IN_FLIGHT_UNDERLAY_EVICT_TIMEOUT),
            )),
            udp_endpoint_pool: Arc::new(crate::udp::UdpEndpointPool::new(consts::MAX_UDP_ENDPOINTS)),
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
    // ══ std::sync::Mutex usage note ════════════════════════════════════════
    // We intentionally use std::sync::Mutex (not tokio::sync::Mutex) for the
    // sessions HashMap.  All critical sections are extremely short (HashMap
    // insert/remove/get, ~ns level) and no .await point is held while the
    // lock is acquired.  Using tokio::sync::Mutex would add unnecessary
    // overhead for these micro-operations.
    //
    // The only caveat: on a multi-threaded tokio runtime, if the thread
    // holding this lock is preempted by the OS scheduler, it may briefly
    // block other worker threads.  Given the microsecond-scale hold times,
    // this is negligible in practice.
    let sessions: Arc<std::sync::Mutex<LruCache<SocketAddr, UnderlaySession>>> =
        Arc::new(std::sync::Mutex::new(LruCache::new(
            NonZeroUsize::new(consts::MAX_UNDERLAY_SESSIONS).unwrap(),
        )));

    // Periodic cleanup: remove sessions that have been idle for longer than the NAT
    // timeout and abort their relay-back tasks so they don't run indefinitely.
    // AbortOnDrop ensures this task is cancelled when run_underlay_packet_loop returns.
    let sessions_cleanup = sessions.clone();
    let _sessions_cleanup_guard = AbortOnDrop(
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                // Collect abort handles while holding the lock, then abort them
                // after releasing it, so abort() is not called under the mutex.
                let to_abort: Vec<tokio::task::AbortHandle> = {
                    let mut guard = sessions_cleanup.lock().unwrap();
                    let mut handles = Vec::new();
                    // Collect expired keys first (LruCache does not have retain())
                    let expired: Vec<SocketAddr> = guard
                        .iter()
                        .filter(|(_, s)| s.last_used.elapsed() >= consts::DEFAULT_NAT_TIMEOUT)
                        .map(|(addr, _)| *addr)
                        .collect();
                    for addr in expired {
                        if let Some(s) = guard.pop(&addr) {
                            if let Some(h) = s.relay_abort {
                                handles.push(h);
                            }
                        }
                    }
                    handles
                };
                for h in to_abort {
                    h.abort();
                }
            }
        })
        .abort_handle(),
    );

    // Limit concurrent underlay packet handler tasks to prevent unbounded task
    // accumulation under high traffic. Each handler may wait up to 100ms in
    // evict() for in-flight underlay auth, and without a cap, thousands of
    // tasks could pile up during a burst, consuming significant memory.
    // Use a fixed cap to keep memory predictable under burst traffic.
    let concurrency_limit = Arc::new(tokio::sync::Semaphore::new(
        consts::MAX_UNDERLAY_HANDLER_CONCURRENCY,
    ));

    while let Some(packet) = rx.recv().await {
        // Acquire a permit before spawning. If all permits are taken, this await will back-pressure
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
    sessions: Arc<std::sync::Mutex<LruCache<SocketAddr, UnderlaySession>>>,
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

    // ── Existing session: decrypt + forward immediately ──
    if let Some(existing) = existing_session {
        // In-place decrypt using cached cipher (plaintext at &payload[SALT_LEN..])
        if let Err(e) = existing.cipher.decrypt_in_place(&mut payload) {
            tracing::debug!(
                "drop invalid underlay packet from {} for target {}: {:?}",
                source,
                existing.target,
                e
            );
            return Ok(());
        }

        // Get or create the UDP endpoint and forward the decrypted payload.
        let ((udp_socket, dial_target), _is_new) = udp_pool
            .get_or_create(
                source,
                crate::udp::UdpEndpointOptions {
                    nat_timeout: consts::DEFAULT_NAT_TIMEOUT,
                    dial_target: existing.target.clone(),
                },
            )
            .await?;

        let send_socket = tokio::net::UdpSocket::from_std(udp_socket)?;
        if let Err(e) = send_socket.send_to(&payload[juicity_underlay::SALT_LEN..], &dial_target).await {
            udp_pool.remove(&source).await;
            // Remove the session; the relay task will be aborted by cleanup.
            let removed = sessions.lock().unwrap().pop(&source);
            if let Some(s) = removed {
                if let Some(h) = s.relay_abort {
                    h.abort();
                }
            }
            return Err(anyhow::anyhow!("underlay send_to {} failed: {:?}", dial_target, e));
        }
        return Ok(());
    }

    // ── New session: auth, decrypt, create endpoint+relay, then insert ──
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

    // Derive subkey directly via HKDF-SHA1.
    // Salt is random per UDP packet, so caching (PSK, salt) → subkey would
    // have near-zero hit rate.  Skip the cache and derive each time.
    let subkey = juicity_underlay::derive_subkey(&auth.psk, &salt)
        .expect("derive_subkey failed: invalid PSK length");
    let cipher = Arc::new(UnderlayCipher::from_subkey(&subkey));

    // In-place decrypt (plaintext at &payload[SALT_LEN..])
    if let Err(e) = cipher.decrypt_in_place(&mut payload) {
        tracing::debug!(
            "drop first underlay packet from {} for target {}: {:?}",
            source,
            auth.metadata.target_addr(),
            e
        );
        return Ok(());
    }
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

    // Convert both sockets BEFORE any spawn so that a conversion failure
    // (e.g. kernel fd exhaustion) cannot produce an orphaned relay-back task.
    let recv_socket_for_relay = if is_new {
        Some(tokio::net::UdpSocket::from_std(udp_socket.try_clone()?)?)
    } else {
        None
    };
    let send_socket = tokio::net::UdpSocket::from_std(udp_socket)?;

    let relay_abort = if let Some(recv_socket) = recv_socket_for_relay {
        let relay_back = server_socket.clone();
        let session_cipher = cipher.clone();
        let sessions_for_task = sessions.clone();
        let udp_pool_for_task = udp_pool.clone();

        // ── SessionGuard ──────────────────────────────────────────────────
        // A Drop guard that ensures the session entry is removed from the
        // sessions map when the relay-back task exits, *even if the task is
        // externally aborted via AbortHandle*.
        //
        // When abort() is called on a running task, tokio drops the future
        // at the next await point — local variables go through Drop, but code
        // after the loop (the manual cleanup below) never executes.
        // SessionGuard's Drop impl runs regardless of how the task terminates:
        //   - Normal exit (loop breaks) → guard is dropped → session removed.
        //   - External abort             → guard is dropped → session removed.
        //   - Panic                      → guard is dropped → session removed.
        //
        // This eliminates the window where an aborted relay task leaves a stale
        // session entry until the periodic cleanup (30s) removes it.
        //
        // Pool cleanup requires async (udp_pool.remove().await), so it cannot
        // live in a Drop guard.  It stays as explicit code after the loop.
        // If the task is aborted before reaching that code, the pool entry
        // will be cleaned up by:
        //   1. The external eviction/abort path (lines 520-522), or
        //   2. The periodic pool cleanup (every 30s).
        struct SessionGuard {
            source: SocketAddr,
            sessions: Arc<std::sync::Mutex<LruCache<SocketAddr, UnderlaySession>>>,
        }
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                let mut guard = self.sessions.lock().unwrap();
                guard.pop(&self.source);
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
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((n, _)) => {
                        let salt = juicity_underlay::generate_underlay_salt();
                        // Pre-allocate with SALT_LEN headroom at front — avoids O(n) shift in encrypt_in_place
                        outbuf.clear();
                        outbuf.reserve(n + juicity_underlay::SALT_LEN + juicity_underlay::TAG_LEN);
                        outbuf.resize(juicity_underlay::SALT_LEN, 0);
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

            // Drop the SessionGuard explicitly before the pool remove so the
            // session map is cleaned up synchronously.  The guard is otherwise
            // dropped by the compiler at the end of the scope, which is fine too.
            drop(_guard);

            // Async pool cleanup — may not run on external abort, but the
            // periodic pool cleanup (every 30s) will handle stale entries.
            udp_pool_for_task.remove(&source).await;
        });
        Some(relay_handle.abort_handle())
    } else {
        None
    };

    let session = UnderlaySession {
        target,
        cipher,
        last_used: std::time::Instant::now(),
        relay_abort,
    };

    // Insert session atomically — LruCache auto-evicts the least recently used
    // entry when at capacity (O(1) amortized). No manual scanning needed.
    let evicted_session: Option<(SocketAddr, Option<tokio::task::AbortHandle>)> = {
        let mut guard = sessions.lock().unwrap();
        // Check if eviction is needed and pop the LRU entry before inserting,
        // so we can retrieve both the key (for udp_pool.remove()) and the abort handle.
        let evicted = if guard.len() >= consts::MAX_UNDERLAY_SESSIONS {
            // peek_lru() returns the least recently used entry without promoting it
            guard.peek_lru().map(|(addr, _)| *addr).and_then(|addr| {
                guard.pop(&addr).map(|s| (addr, s.relay_abort))
            })
        } else {
            None
        };
        // put() will not evict since we already made room if needed
        guard.put(source, session.clone());
        evicted
    };
    if let Some((oldest_addr, relay_abort)) = evicted_session {
        if let Some(h) = relay_abort {
            h.abort()
        }
        udp_pool.remove(&oldest_addr).await;
    }
    tracing::debug!("new underlay session {} -> {}", source, session.target);

    // Send first packet immediately — relay task is already running.
    // Plaintext is at &payload[SALT_LEN..] (salt prefix kept in place).
    if let Err(e) = send_socket.send_to(&payload[juicity_underlay::SALT_LEN..], &dial_target).await {
        udp_pool.remove(&source).await;
        let removed = sessions.lock().unwrap().pop(&source);
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
    // ── Stream acceptance with idle guard ──
    // If a client authenticates but never opens a bidirectional stream, accept_bi()
    // would block forever, leaking the auth task and Arc references.  We wrap it in
    // a 120-second timeout so that even if the peer keeps the QUIC connection alive
    // (e.g. via keep-alive PINGs) without opening streams, we eventually tear down
    // the connection and release its resources.
    const STREAM_ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    loop {
        // Drain completed tasks each iteration to free their resources without blocking.
        while stream_tasks.try_join_next().is_some() {}

        match tokio::time::timeout(STREAM_ACCEPT_TIMEOUT, connection.accept_bi()).await {
            Ok(Ok((send_stream, recv_stream))) => {
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
            Ok(Err(quinn::ConnectionError::ApplicationClosed { .. })) => {
                tracing::debug!("Connection closed by peer: {}", remote_addr);
                break;
            }
            Ok(Err(e)) => {
                tracing::debug!("Accept stream error: {:?}", e);
                break;
            }
            Err(_) => {
                // No stream opened within the timeout window — close the connection
                // to release auth task, Arc references, and QUIC connection memory.
                tracing::debug!(
                    "No stream accepted from {} within {}s timeout",
                    remote_addr,
                    STREAM_ACCEPT_TIMEOUT.as_secs()
                );
                connection.close(
                    VarInt::from_u32(0xfffffff3),
                    b"stream accept timeout",
                );
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
    let (remote_rx, mut remote_tx) = tokio::io::split(remote);
    let (mut quic_tx, quic_rx) = (send_stream, recv_stream);

    // Use 64KB buffered readers for high-throughput bidirectional copy
    let mut remote_rx = tokio::io::BufReader::with_capacity(64 * 1024, remote_rx);
    let mut quic_rx = tokio::io::BufReader::with_capacity(64 * 1024, quic_rx);

    tokio::select! {
        r = tokio::io::copy_buf(&mut remote_rx, &mut quic_tx) => {
            if let Err(e) = r { tracing::debug!("TCP relay remote->quic: {:?}", e); }
        }
        r = tokio::io::copy_buf(&mut quic_rx, &mut remote_tx) => {
            if let Err(e) = r { tracing::debug!("TCP relay quic->remote: {:?}", e); }
        }
    }

    // Gracefully finish the send direction so quinn can clean up the stream
    // state immediately instead of holding it until a timeout or stream reset.
    let _ = quic_tx.finish();

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
    let mut domain_ip_map: IndexMap<(String, u16), (SocketAddr, Instant)> = IndexMap::new();

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
            // Cache the first response address; subsequent responses come from
            // the same outbound target so we avoid re-parsing every datagram.
            let mut cached_addr: Option<protocol::CachedAddr> = None;
            loop {
                match remote.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        // Cache the address on first packet to avoid re-parsing
                        // the address type (string → IPv4/IPv6/Domain) on every
                        // subsequent datagram in this session.
                        let cached = cached_addr.get_or_insert_with(|| {
                            protocol::CachedAddr::from_socket_addr(addr)
                        });
                        // Build header directly into the reusable frame buffer,
                        // eliminating the intermediate Vec allocation.
                        let pkt_len = (n as u16).to_be_bytes();
                        frame.clear();
                        if let Err(e) = protocol::build_trojanc_addr_cached(&mut frame, cached) {
                            tracing::debug!("build_trojanc_addr_cached error: {:?}", e);
                            break;
                        }
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
    domain_ip_map: &mut IndexMap<(String, u16), (SocketAddr, Instant)>,
) -> anyhow::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    let key = (host.to_string(), port);
    // Check cache with TTL expiry
    if let Some((mapped, timestamp)) = domain_ip_map.get(&key) {
        if timestamp.elapsed() < consts::UDP_DNS_CACHE_TTL {
            return Ok(*mapped);
        }
        // TTL expired, remove the stale entry
        domain_ip_map.shift_remove(&key);
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
            domain_ip_map.shift_remove(&oldest_key);
        }
    }
    domain_ip_map.insert(key, (resolved, Instant::now()));
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
