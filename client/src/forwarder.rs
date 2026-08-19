use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicU64 as StdAtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use juicity_common::consts;
use juicity_common::protocol;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::client::JuicityClient;

/// RAII guard: aborts the wrapped task when this guard is dropped.
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Protocol filter for a forward entry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFilter {
    Tcp,
    Udp,
    Both,
}

/// A single forward entry parsed from config
#[derive(Debug, Clone)]
pub struct ForwardEntry {
    /// Local listen address (e.g. "0.0.0.0:1080")
    pub local_addr: SocketAddr,
    /// Target address to forward to (e.g. "1.2.3.4:443")
    pub target: Arc<str>,
    /// Protocol filter
    pub protocol: ProtocolFilter,
}

/// Forwarder listens on local ports and forwards TCP/UDP traffic
/// through a Juicity QUIC connection to the target server.
pub struct Forwarder {
    entries: Vec<ForwardEntry>,
    client: JuicityClient,
}

impl Forwarder {
    /// Create a new Forwarder from config forward entries.
    ///
    /// The config's `forward` field is a `HashMap<String, String>` where:
    /// - key: local address, optionally with protocol suffix (e.g. "0.0.0.0:1080/tcp")
    /// - value: target address (e.g. "1.2.3.4:443")
    pub fn new(
        forward_map: &std::collections::HashMap<String, String>,
        client: JuicityClient,
    ) -> anyhow::Result<Self> {
        let mut entries = Vec::new();

        for (local_raw, target) in forward_map {
            let (addr_str, protocol) = parse_local_addr(local_raw)?;
            let local_addr: SocketAddr = addr_str.parse().map_err(|e| {
                anyhow::anyhow!("invalid forward local address '{}': {}", addr_str, e)
            })?;

            entries.push(ForwardEntry {
                local_addr,
                target: Arc::from(target.as_str()),
                protocol,
            });
        }

        Ok(Self { entries, client })
    }

    /// Start all forward entries.
    /// Each entry spawns its own TCP and/or UDP listener tasks.
    pub async fn start(&self) -> anyhow::Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }

        tracing::info!("Starting forwarder with {} entr(ies)", self.entries.len());

        let mut handles = Vec::new();

        for entry in &self.entries {
            tracing::info!(
                "Forward: local={} {:?} -> remote={}",
                entry.local_addr,
                entry.protocol,
                entry.target
            );

            if entry.protocol == ProtocolFilter::Tcp || entry.protocol == ProtocolFilter::Both {
                let entry = entry.clone();
                let client = self.client.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = start_tcp_forward(entry, client).await {
                        tracing::error!("TCP forward error: {:?}", e);
                    }
                });
                handles.push(handle);
            }

            if entry.protocol == ProtocolFilter::Udp || entry.protocol == ProtocolFilter::Both {
                let entry = entry.clone();
                let client = self.client.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = start_udp_forward(entry, client).await {
                        tracing::error!("UDP forward error: {:?}", e);
                    }
                });
                handles.push(handle);
            }
        }

        // Wait for all forward tasks (they run indefinitely until error)
        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }
}

/// Parse a local address string that may include a protocol suffix.
///
/// Format: `host:port` (defaults to Both) or `host:port/tcp` or `host:port/udp`
fn parse_local_addr(raw: &str) -> anyhow::Result<(&str, ProtocolFilter)> {
    if let Some(slash_pos) = raw.rfind('/') {
        let addr = &raw[..slash_pos];
        let proto = &raw[slash_pos + 1..];
        if proto.eq_ignore_ascii_case("tcp") {
            Ok((addr, ProtocolFilter::Tcp))
        } else if proto.eq_ignore_ascii_case("udp") {
            Ok((addr, ProtocolFilter::Udp))
        } else {
            anyhow::bail!(
                "unknown protocol '{}' in forward address '{}', expected tcp/udp",
                proto,
                raw
            )
        }
    } else {
        // No protocol suffix: default to both TCP and UDP
        Ok((raw, ProtocolFilter::Both))
    }
}

/// Start a TCP forwarder for a single entry.
/// Listens on the local TCP port, accepts connections, and forwards
/// each one through the Juicity QUIC connection to the target.
async fn start_tcp_forward(entry: ForwardEntry, client: JuicityClient) -> anyhow::Result<()> {
    let listener = TcpListener::bind(entry.local_addr).await?;
    tracing::info!(
        "TCP forward listening on {} -> {}",
        entry.local_addr,
        entry.target
    );

    // Limit concurrent inbound TCP connections to avoid unbounded memory growth
    // during connection bursts, matching the UDP concurrency limit and the
    // local proxy in local.rs.
    let sem = Arc::new(Semaphore::new(consts::MAX_CONCURRENT_TCP_CONNECTIONS));

    loop {
        // Acquire a permit before accepting; this blocks new accepts when the
        // limit is reached, providing back-pressure at the OS TCP accept queue.
        let permit = sem.clone().acquire_owned().await?;
        let (stream, peer_addr) = listener.accept().await?;
        let client = client.clone();
        let target = entry.target.clone();

        tokio::spawn(async move {
            let _permit = permit; // held for the lifetime of the connection
            tracing::info!(
                client_addr = %peer_addr,
                target = %target,
                protocol = "tcp",
                "TCP forward accepted"
            );

            if let Err(e) = forward_tcp_connection(stream, &target, &client).await {
                tracing::info!(
                    error = %e,
                    direction = "connection",
                    protocol = "tcp",
                    "TCP forward connection error"
                );
            }
        });
    }
}

/// Forward a single TCP connection through the Juicity QUIC connection.
async fn forward_tcp_connection(
    mut local_stream: TcpStream,
    target: &str,
    client: &JuicityClient,
) -> anyhow::Result<()> {
    // Parse target into host and port
    let (host, port) = parse_target(target)?;

    // Open a TCP stream through the Juicity QUIC connection
    let (mut quic_send, quic_recv) = client.open_tcp_stream(&host, port).await?;

    // Bidirectional copy between local TCP and QUIC stream
    let (local_rx, mut local_tx) = local_stream.split();

    // Use 16KB buffered readers for high-throughput bidirectional copy.
    let mut local_rx = tokio::io::BufReader::with_capacity(16 * 1024, local_rx);
    let mut quic_recv = tokio::io::BufReader::with_capacity(16 * 1024, quic_recv);

    let (r1, r2) = tokio::join!(
        tokio::io::copy_buf(&mut local_rx, &mut quic_send),
        tokio::io::copy_buf(&mut quic_recv, &mut local_tx),
    );
    if let Err(e) = r1 {
        tracing::info!(
            error = %e,
            direction = "local->quic",
            protocol = "tcp",
            "TCP forward local->quic error"
        );
    }
    if let Err(e) = r2 {
        tracing::info!(
            error = %e,
            direction = "quic->local",
            protocol = "tcp",
            "TCP forward quic->local error"
        );
    }

    // Gracefully finish the send direction so quinn can clean up the stream
    // state immediately instead of holding it until a timeout or stream reset.
    let _ = quic_send.finish();

    Ok(())
}

// ============================================================
// UDP Forward — single-task event loop with DashMap
// ============================================================

/// A UDP session that holds the QUIC writer channel for a given source address.
struct UdpSession {
    /// Unique session ID, used by the supervisor to verify it is removing the
    /// correct entry from the sessions map (prevents races with session replacement).
    id: u64,
    /// Last time a datagram was forwarded through this session.
    /// Stored as epoch millis (AtomicU64) so updates don't need a Mutex.
    last_used_epoch_ms: StdAtomicU64,
    /// Sender channel to push outbound datagrams to the QUIC writer task.
    /// `try_send` is used on the hot path to avoid blocking the receive loop.
    tx: tokio::sync::mpsc::Sender<Bytes>,
}

impl UdpSession {
    fn touch(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_used_epoch_ms.store(now, Ordering::Relaxed);
    }

    fn last_used(&self) -> Instant {
        let ms = self.last_used_epoch_ms.load(Ordering::Relaxed);
        // Convert epoch millis back to Instant via UNIX_EPOCH.
        // This is approximate but sufficient for idle detection.
        Instant::now() - std::time::Duration::from_millis(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64)
                .saturating_sub(ms),
        )
    }
}

/// Start a UDP forwarder for a single entry.
///
/// Uses a single-task event loop with DashMap for session lookup.
/// The hot path (existing session, `try_send` success) executes entirely
/// inline without spawning a task or acquiring a Mutex.
async fn start_udp_forward(entry: ForwardEntry, client: JuicityClient) -> anyhow::Result<()> {
    let socket = Arc::new(UdpSocket::bind(entry.local_addr).await?);
    tracing::info!(
        "UDP forward listening on {} -> {}",
        entry.local_addr,
        entry.target
    );

    let (host, port) = parse_target(&entry.target)?;
    let host = Arc::from(host);

    // DashMap: sharded concurrent HashMap, no global Mutex on the hot path.
    let sessions: Arc<DashMap<SocketAddr, UdpSession>> = Arc::new(DashMap::new());

    // Monotonically increasing session ID counter.
    let session_seq = AtomicU64::new(1);

    // Periodic cleanup: remove sessions whose writer channel has been closed
    // or which have been idle beyond the NAT timeout.
    // Uses DashMap::retain which locks shards individually, avoiding a global pause.
    let _cleanup_guard = AbortOnDrop(
        tokio::spawn({
            let sessions = sessions.clone();
            async move {
                let mut interval =
                    tokio::time::interval(consts::CLIENT_UDP_SESSION_CLEANUP_INTERVAL);
                loop {
                    interval.tick().await;
                    let idle_cutoff = Instant::now() - consts::CLIENT_UDP_SESSION_IDLE_TIMEOUT;
                    sessions.retain(|_, s| !s.tx.is_closed() && s.last_used() > idle_cutoff);
                }
            }
        })
        .abort_handle(),
    );

    // CancellationToken: cancelled when start_udp_forward returns (via drop_guard).
    let cancel = CancellationToken::new();
    let _cancel_guard = cancel.clone().drop_guard();

    let mut buf = vec![0u8; consts::ETHERNET_MTU];

    // ── Single-task event loop: no per-packet spawn ──
    loop {
        let (n, src_addr) = socket.recv_from(&mut buf).await?;
        let data = Bytes::copy_from_slice(&buf[..n]);

        // ── Fast path: session exists, try_send (non-blocking) ──
        if let Some(session) = sessions.get(&src_addr) {
            session.touch();
            if session.tx.try_send(data.clone()).is_ok() {
                continue;
            }
            // Channel full or closed — fall through to session recreation.
            // The writer/reader tasks will detect the closed channel and exit
            // on their own; we just replace the entry below.
            drop(session); // release DashMap ref before insert
        }

        // ── Slow path: create a new session (low frequency) ──
        let (mut send, mut recv) = match client.open_udp_stream(&host, port, &data[..]).await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::info!(
                    error = %e,
                    protocol = "udp",
                    "UDP forward stream open error"
                );
                continue;
            }
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(256);
        let session_id = session_seq.fetch_add(1, Ordering::Relaxed);

        // Send the first datagram directly on the send stream (already opened above).
        // Reuse the scratch buffer approach from the old writer task.
        {
            let mut addr_buf = Vec::with_capacity(32);
            if let Err(e) =
                JuicityClient::send_udp_datagram(&mut send, &host, port, &data[..], &mut addr_buf)
                    .await
            {
                tracing::info!(
                    error = %e,
                    protocol = "udp",
                    "UDP forward first datagram send error"
                );
                continue;
            }
        }

        sessions.insert(
            src_addr,
            UdpSession {
                id: session_id,
                last_used_epoch_ms: StdAtomicU64::new(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                ),
                tx: tx.clone(),
            },
        );

        // Spawn writer task: reads from channel and sends via QUIC.
        // One task per session (not per packet).
        let writer_handle = tokio::spawn({
            let host = host.clone();
            async move {
                let mut addr_buf = Vec::with_capacity(32);
                // RAII guard: ensure send.finish() is called even when this task is
                // aborted (e.g. via cancel).  Without this, the QUIC send stream
                // would be left in a half-closed state until the connection idle
                // timeout fires (up to 600s), holding stream resources unnecessarily.
                struct SendGuard {
                    send: Option<quinn::SendStream>,
                }
                impl Drop for SendGuard {
                    fn drop(&mut self) {
                        if let Some(ref mut s) = self.send {
                            let _ = s.finish();
                        }
                    }
                }
                let mut guard = SendGuard { send: Some(send) };
                loop {
                    match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, rx.recv()).await {
                        Ok(Some(datagram)) => {
                            if JuicityClient::send_udp_datagram(
                                guard.send.as_mut().unwrap(),
                                &host,
                                port,
                                &datagram[..],
                                &mut addr_buf,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break, // NAT timeout
                    }
                }
            }
        });

        // Spawn reader task: reads responses from QUIC and sends back to local UDP.
        let reader_handle = tokio::spawn({
            let socket = socket.clone();
            async move {
                let mut recv_buf = Vec::with_capacity(65535);
                loop {
                    match read_one_udp_response(&mut recv, &mut recv_buf).await {
                        Ok(()) => {
                            if socket.send_to(&recv_buf, src_addr).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // Supervisor: abort the non-finishing side when one task exits,
        // or abort both immediately if the parent forwarder has been cancelled.
        tokio::spawn({
            let sessions = sessions.clone();
            let cancel_token = cancel.clone();
            async move {
                let mut writer = writer_handle;
                let mut reader = reader_handle;
                tokio::select! {
                    _ = &mut writer => {
                        reader.abort();
                        let _ = reader.await;
                    }
                    _ = &mut reader => {
                        writer.abort();
                        let _ = writer.await;
                    }
                    _ = cancel_token.cancelled() => {
                        writer.abort();
                        reader.abort();
                    }
                }
                // Remove session only if the entry hasn't been replaced by a
                // new session for the same src_addr (verified by session_id).
                if let Some(entry) = sessions.get(&src_addr) {
                    if entry.id == session_id {
                        drop(entry); // release ref before remove
                        sessions.remove(&src_addr);
                    }
                }
            }
        });
    }
}

/// Read one UDP response from a QUIC recv stream.
/// Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
///
/// Uses the caller-provided `buf` (pre-allocated with sufficient capacity) to
/// avoid per-packet heap allocation inside a hot loop.
async fn read_one_udp_response(
    recv: &mut quinn::RecvStream,
    buf: &mut Vec<u8>,
) -> anyhow::Result<()> {
    // Discard the per-response address — the session already knows the target
    tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        protocol::read_trojanc_addr_async(recv),
    )
    .await??;

    let mut len_buf = [0u8; 2];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut len_buf)).await??;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    buf.resize(pkt_len, 0);
    tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        recv.read_exact(&mut buf[..pkt_len]),
    )
    .await??;

    Ok(())
}

/// Parse a "host:port" target string into (host, port), properly handling IPv6 addresses like [::1]:443.
fn parse_target(target: &str) -> anyhow::Result<(String, u16)> {
    juicity_common::link::parse_host_port(target).map_err(|e| anyhow::anyhow!("{}", e))
}
