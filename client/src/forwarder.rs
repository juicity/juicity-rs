use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use juicity_common::consts;
use juicity_common::protocol;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Semaphore};
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
    // during connection bursts, matching the UDP Semaphore(256) below and the
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
            tracing::debug!(
                "TCP forward: accepted from {}, forwarding to {}",
                peer_addr,
                target
            );

            if let Err(e) = forward_tcp_connection(stream, &target, &client).await {
                tracing::debug!("TCP forward connection error: {:?}", e);
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

    // Use 16KB buffered readers (reduced from 64KB) for high-throughput bidirectional copy.
    // 64KB × 2 × 256 concurrent connections = 32MB; 16KB × 2 × 256 = 8MB — saves 24MB.
    let mut local_rx = tokio::io::BufReader::with_capacity(16 * 1024, local_rx);
    let mut quic_recv = tokio::io::BufReader::with_capacity(16 * 1024, quic_recv);

    tokio::select! {
        r = tokio::io::copy_buf(&mut local_rx, &mut quic_send) => {
            if let Err(e) = r {
                tracing::debug!("TCP forward local->quic: {:?}", e);
            }
        }
        r = tokio::io::copy_buf(&mut quic_recv, &mut local_tx) => {
            if let Err(e) = r {
                tracing::debug!("TCP forward quic->local: {:?}", e);
            }
        }
    }

    // Gracefully finish the send direction so quinn can clean up the stream
    // state immediately instead of holding it until a timeout or stream reset.
    let _ = quic_send.finish();

    Ok(())
}

/// Start a UDP forwarder for a single entry.
/// Listens on the local UDP port, and forwards datagrams through
/// the Juicity QUIC connection to the target.
async fn start_udp_forward(entry: ForwardEntry, client: JuicityClient) -> anyhow::Result<()> {
    let socket = Arc::new(UdpSocket::bind(entry.local_addr).await?);
    tracing::info!(
        "UDP forward listening on {} -> {}",
        entry.local_addr,
        entry.target
    );

    let (host, port) = parse_target(&entry.target)?;
    let host = Arc::from(host);

    // We use a shared state to track UDP sessions per source address.
    // Each unique source address gets its own QUIC bidirectional stream.
    let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Monotonically increasing session ID counter. Each new session gets a unique
    // ID so that supervisor tasks can verify they are removing their own session
    // entry, preventing races where an old supervisor removes a newly created
    // session for the same source address.
    let session_seq = Arc::new(AtomicU64::new(1));

    // Periodic cleanup: remove sessions whose writer channel has been closed.
    // AbortOnDrop ensures this task is cancelled when start_udp_forward returns
    // (either on error or listener close), preventing the sessions Arc from being
    // kept alive indefinitely by an orphaned background task.
    let sessions_cleanup = sessions.clone();
    let _cleanup_guard = AbortOnDrop(
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                sessions_cleanup
                    .lock()
                    .await
                    .retain(|_, s| !s.tx.is_closed());
            }
        })
        .abort_handle(),
    );

    // CancellationToken: cancelled when start_udp_forward returns (via drop_guard).
    // All session supervisor tasks select! on this token to exit promptly instead of
    // waiting up to DEFAULT_NAT_TIMEOUT for QUIC I/O to time out.
    let cancel = CancellationToken::new();
    let _cancel_guard = cancel.clone().drop_guard();

    let concurrency_limit = Arc::new(tokio::sync::Semaphore::new(256));

    let mut buf = vec![0u8; consts::ETHERNET_MTU];

    loop {
        let (n, src_addr) = socket.recv_from(&mut buf).await?;
        let data = Bytes::copy_from_slice(&buf[..n]);

        // Back-pressure: if all 256 permits are taken, wait here instead of
        // spawning yet another task. This naturally throttles the receive loop
        // to match the processing capacity.
        let permit = concurrency_limit.clone().acquire_owned().await;
        let socket = Arc::clone(&socket);
        let client = client.clone();
        let host = Arc::clone(&host);
        let sessions = Arc::clone(&sessions);
        let session_seq = Arc::clone(&session_seq);
        let cancel = cancel.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_udp_datagram(
                socket,
                sessions,
                session_seq,
                src_addr,
                data,
                host,
                port,
                &client,
                cancel,
            )
            .await
            {
                tracing::debug!("UDP forward datagram error: {:?}", e);
            }
            drop(permit);
        });
    }
}

/// A UDP session that holds the QUIC stream for a given source address.
struct UdpSession {
    /// Unique session ID, used by the supervisor to verify it is removing the
    /// correct entry from the sessions map (prevents races with session replacement).
    id: u64,
    /// Sender channel to push outbound datagrams to the QUIC writer task
    tx: tokio::sync::mpsc::Sender<Bytes>,
}

/// Handle an incoming UDP datagram: find or create a session, then forward.
async fn handle_udp_datagram(
    socket: Arc<UdpSocket>,
    sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession>>>,
    session_seq: Arc<AtomicU64>,
    src_addr: SocketAddr,
    data: Bytes,
    host: Arc<str>,
    port: u16,
    client: &JuicityClient,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // Check if we already have a session for this source
    let existing = {
        let guard = sessions.lock().await;
        guard.get(&src_addr).map(|s| (s.id, s.tx.clone()))
    };

    if let Some((session_id, tx)) = existing {
        // Session exists, send datagram through it.
        // Bytes is Arc-based, so clone is cheap (refcount increment).
        if tx.send(data.clone()).await.is_ok() {
            return Ok(());
        }
        // Session is dead — remove it only if the entry hasn't been replaced
        // by a concurrent session creation. Using session_id prevents this remove
        // from deleting a newly created session for the same src_addr.
        {
            let mut guard = sessions.lock().await;
            if let Some(s) = guard.get(&src_addr) {
                if s.id == session_id {
                    guard.remove(&src_addr);
                }
            }
        }

        // Re-create session below with a new QUIC stream
    }

    // Create a new session: open a QUIC UDP stream with the first datagram
    let (mut send, mut recv) = client.open_udp_stream(&host, port, &data[..]).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let session_id = session_seq.fetch_add(1, Ordering::Relaxed);

    // Insert session
    {
        let mut guard = sessions.lock().await;
        guard.insert(
            src_addr,
            UdpSession {
                id: session_id,
                tx: tx.clone(),
            },
        );
    }

    let sessions_clone = sessions.clone();
    let socket_clone = socket.clone();

    // Spawn writer task: reads from channel and sends via QUIC
    let writer_handle = tokio::spawn(async move {
        // Reusable scratch buffer to avoid per-packet heap allocation for address headers.
        let mut addr_buf = Vec::with_capacity(32);
        loop {
            match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, rx.recv()).await {
                Ok(Some(datagram)) => {
                    if JuicityClient::send_udp_datagram(
                        &mut send,
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
        let _ = send.finish();
    });

    // Spawn reader task: reads responses from QUIC and sends back to local UDP
    let reader_handle = tokio::spawn(async move {
        // Pre-allocate a reusable buffer (max UDP datagram size) to avoid
        // per-packet heap allocation inside the hot loop.
        let mut recv_buf = Vec::with_capacity(65535);
        loop {
            match read_one_udp_response(&mut recv, &mut recv_buf).await {
                Ok(()) => {
                    if socket_clone.send_to(&recv_buf, src_addr).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Supervisor: abort the non-finishing side when one task exits, or abort both
    // immediately if the parent forwarder has been cancelled (start_udp_forward exited).
    // Using select! instead of join! ensures the reader's NAT timeout does not block
    // cleanup when the writer exits early.
    //
    // Uses session_id to verify the entry matches before removing, preventing a race
    // where an old supervisor removes a new session created for the same src_addr.
    tokio::spawn(async move {
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
            _ = cancel.cancelled() => {
                writer.abort();
                reader.abort();
            }
        }
        let mut guard = sessions_clone.lock().await;
        if let Some(s) = guard.get(&src_addr) {
            if s.id == session_id {
                guard.remove(&src_addr);
            }
        }
    });

    Ok(())
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
