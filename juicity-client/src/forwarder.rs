use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use juicity_common::consts;
use juicity_common::protocol;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;

use crate::client::JuicityClient;

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
    pub fn new(forward_map: &std::collections::HashMap<String, String>, client: JuicityClient) -> anyhow::Result<Self> {
        let mut entries = Vec::new();

        for (local_raw, target) in forward_map {
            let (addr_str, protocol) = parse_local_addr(local_raw)?;
            let local_addr: SocketAddr = addr_str
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid forward local address '{}': {}", addr_str, e))?;

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
        match proto.to_lowercase().as_str() {
            "tcp" => Ok((addr, ProtocolFilter::Tcp)),
            "udp" => Ok((addr, ProtocolFilter::Udp)),
            _ => anyhow::bail!(
                "unknown protocol '{}' in forward address '{}', expected tcp/udp",
                proto,
                raw
            ),
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

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let client = client.clone();
        let target = entry.target.clone();

        tokio::spawn(async move {
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
    let (mut quic_send, mut quic_recv) = client.open_tcp_stream(&host, port).await?;

    // Bidirectional copy between local TCP and QUIC stream
    let (mut local_rx, mut local_tx) = local_stream.split();

    tokio::select! {
        r = tokio::io::copy(&mut local_rx, &mut quic_send) => {
            if let Err(e) = r {
                tracing::debug!("TCP forward local->quic: {:?}", e);
            }
        }
        r = tokio::io::copy(&mut quic_recv, &mut local_tx) => {
            if let Err(e) = r {
                tracing::debug!("TCP forward quic->local: {:?}", e);
            }
        }
    }

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

    let mut buf = vec![0u8; consts::ETHERNET_MTU];

    loop {
        let (n, src_addr) = socket.recv_from(&mut buf).await?;
        let data = Bytes::copy_from_slice(&buf[..n]);

        let socket = Arc::clone(&socket);
        let client = client.clone();
        let host = Arc::clone(&host);
        let sessions = Arc::clone(&sessions);

        tokio::spawn(async move {
            if let Err(e) = handle_udp_datagram(
                socket, sessions, src_addr, data, host, port, &client,
            )
            .await
            {
                tracing::debug!("UDP forward datagram error: {:?}", e);
            }
        });
    }
}

/// A UDP session that holds the QUIC stream for a given source address.
struct UdpSession {
    /// Sender channel to push outbound datagrams to the QUIC writer task
    tx: tokio::sync::mpsc::Sender<Bytes>,
}

/// Handle an incoming UDP datagram: find or create a session, then forward.
async fn handle_udp_datagram(
    socket: Arc<UdpSocket>,
    sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession>>>,
    src_addr: SocketAddr,
    data: Bytes,
    host: Arc<str>,
    port: u16,
    client: &JuicityClient,
) -> anyhow::Result<()> {
    // Check if we already have a session for this source
    let existing_tx = {
        let guard = sessions.lock().await;
        guard.get(&src_addr).map(|s| s.tx.clone())
    };

    if let Some(tx) = existing_tx {
        // Session exists, send datagram through it.
        // Bytes is Arc-based, so clone is cheap (refcount increment).
        if tx.send(data.clone()).await.is_ok() {
            return Ok(());
        }
        // Session is dead, remove it
        sessions.lock().await.remove(&src_addr);

        // Re-create session below with a new QUIC stream
    }

    // Create a new session: open a QUIC UDP stream with the first datagram
    let (mut send, mut recv) = client.open_udp_stream(&host, port, &data[..]).await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(256);

    // Insert session
    {
        let mut guard = sessions.lock().await;
        guard.insert(
            src_addr,
            UdpSession { tx: tx.clone() },
        );
    }

    let sessions_clone = sessions.clone();
    let socket_clone = socket.clone();

    // Spawn writer task: reads from channel and sends via QUIC
    let writer_handle = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, rx.recv()).await {
                Ok(Some(datagram)) => {
                    if JuicityClient::send_udp_datagram(&mut send, &host, port, &datagram[..])
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
        loop {
            match read_one_udp_response(&mut recv).await {
                Ok(payload) => {
                    if socket_clone.send_to(&payload, src_addr).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Clean up when either task finishes
    tokio::spawn(async move {
        let _ = tokio::join!(writer_handle, reader_handle);
        sessions_clone.lock().await.remove(&src_addr);
    });

    Ok(())
}

/// Read one UDP response from a QUIC recv stream.
/// Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
async fn read_one_udp_response(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<Vec<u8>> {
    // Discard the per-response address — the session already knows the target
    tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        protocol::read_trojanc_addr_async(recv),
    )
    .await??;

    let mut len_buf = [0u8; 2];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut len_buf)).await??;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; pkt_len];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut payload)).await??;

    Ok(payload)
}

/// Parse a "host:port" target string into (host, port).
fn parse_target(target: &str) -> anyhow::Result<(String, u16)> {
    // Use rsplitn to handle IPv6 addresses like [::1]:443
    let parts: Vec<&str> = target.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid target address '{}', expected host:port", target);
    }
    let port: u16 = parts[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid port in target '{}': {}", target, e))?;
    let host = parts[1].to_string();
    Ok((host, port))
}
