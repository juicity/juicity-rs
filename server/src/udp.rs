use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use juicity_common::consts;
use juicity_common::protocol;
use moka::sync::Cache;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

/// Relay UDP responses to a Juicity stream.
pub(crate) async fn relay_responses<W: AsyncWrite + Unpin>(
    remote: &tokio::net::UdpSocket,
    writer: &mut W,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; consts::ETHERNET_MTU];
    let mut frame = Vec::with_capacity(264 + consts::ETHERNET_MTU);

    loop {
        match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, remote.recv_from(&mut buf)).await {
            Ok(Ok((n, addr))) => {
                frame.clear();
                let response_addr = protocol::CachedAddr::from_socket_addr(addr);
                protocol::build_trojanc_addr_cached(&mut frame, &response_addr)?;
                frame.extend_from_slice(&(n as u16).to_be_bytes());
                frame.extend_from_slice(&buf[..n]);
                writer.write_all(&frame).await?;
            }
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => return Ok(()),
        }
    }
}

/// Options for creating a UDP endpoint
pub struct UdpEndpointOptions {
    pub nat_timeout: Duration,
    pub dial_target: String,
}

/// A UDP endpoint representing a full-cone NAT session
///
/// Stores a `tokio::net::UdpSocket` directly (instead of `std::net::UdpSocket`)
/// so that callers on the hot path can use it immediately without an expensive
/// `tokio::net::UdpSocket::from_std()` syscall registration per packet.
pub struct UdpEndpoint {
    /// Tokio UDP socket — cached here to avoid per-packet `from_std()`.
    /// Wrapped in `Arc` so that cloning on the hot path is just a
    /// refcount increment — no heap allocation and no `try_clone()` syscall.
    pub socket: Arc<tokio::net::UdpSocket>,
    /// Stored as `Arc<str>` so that cloning on the fast path is just a
    /// refcount increment — no heap allocation per packet.
    pub dial_target: Arc<str>,
}

impl UdpEndpoint {
    /// Create a new UDP endpoint bound to a random port.
    /// Uses tokio::net::UdpSocket for async bind to avoid blocking the runtime.
    pub async fn new(options: UdpEndpointOptions) -> anyhow::Result<Self> {
        // Use "[::]:0" (IPv6 any) for dual-stack binding.
        // On Linux, binding to "[::]" by default has IPV6_V6ONLY=false,
        // accepting both IPv4 and IPv6 connections.
        let socket = tokio::net::UdpSocket::bind("[::]:0").await?;
        Ok(Self {
            socket: Arc::new(socket),
            dial_target: Arc::from(options.dial_target.as_str()),
        })
    }
}

/// Pool of UDP endpoints for full-cone NAT
///
/// Uses [`moka::sync::Cache`] internally with `time_to_idle` for automatic
/// TTL-based eviction of idle entries, replacing the prior `Mutex<LruCache>`
/// which caused global lock contention on the hot read path.
///
/// # Lock behaviour
///
/// * **Hot path** (`get_socket`): uses moka's sharded internal locking for
///   reads — concurrent reads to different keys never contend.
/// * **Per-addr creation** (`get_or_create`): per-address `Mutex<HashSet>`
///   arbitration so concurrent creations for *different* addresses proceed
///   in parallel, while duplicate creation for the *same* address is
///   serialised via a per-address [`Notify`].
pub struct UdpEndpointPool {
    /// Moka cache with automatic time_to_idle eviction and sharded locking.
    inner: Cache<SocketAddr, Arc<UdpEndpoint>>,
    /// Tracks which addresses currently have an in-flight `UdpEndpoint::new`.
    creating: Mutex<HashSet<SocketAddr>>,
    /// Maps addresses being created to a [`Notify`] that will be signalled
    /// when creation completes (successfully or otherwise).
    notify_map: Mutex<HashMap<SocketAddr, Arc<Notify>>>,
}

impl UdpEndpointPool {
    pub fn new(max_size: u64) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_size)
                // Auto-evict entries idle longer than NAT timeout.
                .time_to_idle(consts::DEFAULT_NAT_TIMEOUT)
                .build(),
            creating: Mutex::new(HashSet::new()),
            notify_map: Mutex::new(HashMap::new()),
        }
    }

    /// Fast path: grab a cloned socket + dial_target of an existing endpoint.
    ///
    /// Returns `(Arc<tokio::net::UdpSocket>, Arc<str>)` — the socket is
    /// already wrapped in tokio, **no** `from_std()` registration needed.
    /// Both fields are `Arc`-wrapped, so cloning them is a refcount increment.
    ///
    /// Uses moka's sharded internal locking — concurrent reads to different
    /// keys proceed in parallel.
    pub fn get_socket(&self, addr: &SocketAddr) -> Option<(Arc<tokio::net::UdpSocket>, Arc<str>)> {
        let endpoint = self.inner.get(addr)?;
        let socket = endpoint.socket.clone();
        let dial_target = endpoint.dial_target.clone();
        Some((socket, dial_target))
    }

    /// Get or create a UDP endpoint for the given address.
    ///
    /// Returns `((Arc<tokio::net::UdpSocket>, Arc<str>), is_new)` where:
    /// * The socket is a **tokio** socket, ready for immediate use without
    ///   `from_std()`.
    /// * `is_new` is `true` if a brand-new `UdpEndpoint` was created (so the
    ///   caller knows to also spawn a relay-back reader task).
    ///
    /// # Concurrency design
    ///
    /// 1. **Fast path**: check moka cache (sharded, no global lock).
    /// 2. **Per-addr creation lock**: attempt to insert `addr` into the
    ///    [`creating`](Self::creating) set.  If another task is already
    ///    creating for this address, we register a per-addr [`Notify`] and
    ///    wait — without blocking creation for *other* addresses.
    /// 3. **Creation**: the designated caller calls `UdpEndpoint::new`,
    ///    inserts the result into the moka cache, removes the address from
    ///    `creating`, and notifies any waiters.
    /// 4. **Post-wakeup**: waiters retry the cache lookup.
    pub async fn get_or_create(
        &self,
        addr: SocketAddr,
        options: UdpEndpointOptions,
    ) -> anyhow::Result<((Arc<tokio::net::UdpSocket>, Arc<str>), bool)> {
        struct CreationGuard<'a> {
            addr: SocketAddr,
            creating: &'a Mutex<HashSet<SocketAddr>>,
            notify_map: &'a Mutex<HashMap<SocketAddr, Arc<Notify>>>,
            notify: Arc<Notify>,
        }

        impl Drop for CreationGuard<'_> {
            fn drop(&mut self) {
                self.creating
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.addr);
                self.notify_map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.addr);
                self.notify.notify_waiters();
            }
        }

        // Use a loop instead of recursion to avoid infinitely sized futures
        // (Rust does not allow recursive async fn calls without boxing).
        loop {
            // ── Fast path: check moka cache (sharded, no global lock). ──
            if let Some(endpoint) = self.inner.get(&addr) {
                let socket = endpoint.socket.clone();
                let dial_target = endpoint.dial_target.clone();
                return Ok(((socket, dial_target), false));
            }

            // ── Per-addr creation arbitration via Mutex<HashSet> ──
            let notify = Arc::new(Notify::new());

            if self
                .creating
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(addr)
            {
                // ── We are the creator ──
                self.notify_map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(addr, notify.clone());

                let _creation_guard = CreationGuard {
                    addr,
                    creating: &self.creating,
                    notify_map: &self.notify_map,
                    notify,
                };

                // Double-check: another task may have inserted a fresh endpoint
                // while we waited for the HashSet insert.
                if let Some(endpoint) = self.inner.get(&addr) {
                    let socket = endpoint.socket.clone();
                    let dial_target = endpoint.dial_target.clone();
                    return Ok(((socket, dial_target), false));
                }

                // Confirmed: no valid endpoint exists, safely create one.
                let result = UdpEndpoint::new(options).await;

                match result {
                    Ok(endpoint) => {
                        let dial_target = endpoint.dial_target.clone();
                        let socket = endpoint.socket.clone();
                        self.inner.insert(addr, Arc::new(endpoint));
                        return Ok(((socket, dial_target), true));
                    }
                    Err(e) => return Err(e),
                }
            }

            // ── Someone else is creating this endpoint — wait for them. ──
            let wait_notify = {
                let mut map = self.notify_map.lock().unwrap_or_else(|e| e.into_inner());
                map.entry(addr)
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone()
            };

            // Double-check: the creator might have completed between our
            // `creating.insert` returning false and registering above.
            if !self
                .creating
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&addr)
            {
                // Creator finished — check cache and retry from the top.
                continue;
            }

            // Wait for the creator to finish.
            wait_notify.notified().await;

            // Re-check the cache after wakeup — retry from top.
            continue;
        }
    }

    /// Remove a UDP endpoint from the cache by address.
    ///
    /// Typically called when a connection is closed or a send error is
    /// detected, ensuring stale endpoints do not linger in the pool.
    pub fn remove(&self, addr: &SocketAddr) {
        self.inner.invalidate(addr);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use juicity_common::protocol;
    use tokio::io::{AsyncReadExt, DuplexStream};

    use super::relay_responses;

    async fn read_response(reader: &mut DuplexStream) -> (String, u16, Vec<u8>) {
        let (host, port) = protocol::read_trojanc_addr_async(reader)
            .await
            .expect("response address should decode");
        let mut len = [0u8; 2];
        reader
            .read_exact(&mut len)
            .await
            .expect("response length should decode");
        let mut payload = vec![0u8; u16::from_be_bytes(len) as usize];
        reader
            .read_exact(&mut payload)
            .await
            .expect("response payload should decode");
        (host, port, payload)
    }

    #[tokio::test]
    async fn relay_responses_keeps_each_packet_source_address() {
        let relay = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("relay socket should bind");
        let relay_addr = relay
            .local_addr()
            .expect("relay address should be available");
        let source_one = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("first source socket should bind");
        let source_two = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("second source socket should bind");
        let source_one_addr = source_one
            .local_addr()
            .expect("first source address should be available");
        let source_two_addr = source_two
            .local_addr()
            .expect("second source address should be available");
        let (mut reader, mut writer) = tokio::io::duplex(4096);

        let relay_task = tokio::spawn(async move { relay_responses(&relay, &mut writer).await });

        for (source, address, payload) in [
            (&source_one, source_one_addr, b"first".as_slice()),
            (&source_two, source_two_addr, b"second".as_slice()),
            (&source_one, source_one_addr, b"third".as_slice()),
        ] {
            source
                .send_to(payload, relay_addr)
                .await
                .expect("response packet should send");
            let (host, port, actual_payload) =
                tokio::time::timeout(Duration::from_secs(1), read_response(&mut reader))
                    .await
                    .expect("response should arrive before timeout");
            assert_eq!(host, address.ip().to_string());
            assert_eq!(port, address.port());
            assert_eq!(actual_payload, payload);
        }

        relay_task.abort();
        let result = tokio::time::timeout(Duration::from_secs(1), relay_task)
            .await
            .expect("relay task should stop after abort");
        assert!(result
            .expect_err("aborted relay task should be cancelled")
            .is_cancelled());
    }
}
