use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::{DashMap, DashSet};
use lru::LruCache;
use tokio::sync::Mutex;
use tokio::sync::Notify;

/// Options for creating a UDP endpoint
pub struct UdpEndpointOptions {
    pub nat_timeout: Duration,
    pub dial_target: String,
}

/// A UDP endpoint representing a full-cone NAT session
pub struct UdpEndpoint {
    pub socket: std::net::UdpSocket,
    /// Stored as `Arc<str>` so that cloning on the fast path is just a
    /// refcount increment — no heap allocation per packet.
    pub dial_target: Arc<str>,
    last_used: Instant,
    nat_timeout: Duration,
}

impl UdpEndpoint {
    /// Create a new UDP endpoint bound to a random port.
    /// Uses tokio::net::UdpSocket for async bind to avoid blocking the runtime.
    pub async fn new(options: UdpEndpointOptions) -> anyhow::Result<Self> {
        // Use "[::]:0" (IPv6 any) for dual-stack binding.
        // On Linux, binding to "[::]" by default has IPV6_V6ONLY=false,
        // accepting both IPv4 and IPv6 connections.
        let tokio_socket = tokio::net::UdpSocket::bind("[::]:0").await?;
        let socket = tokio_socket.into_std()?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket,
            dial_target: Arc::from(options.dial_target.as_str()),
            last_used: Instant::now(),
            nat_timeout: options.nat_timeout,
        })
    }

    pub fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.last_used) > self.nat_timeout
    }

    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

/// Pool of UDP endpoints for full-cone NAT
///
/// # Per-entry creation locking
///
/// Instead of a single global `create_lock` (which serialises all
/// `get_or_create` calls regardless of target address), this implementation
/// uses a [`DashSet<SocketAddr>`] to track **per-address** creation state.
/// Concurrent calls for different addresses proceed in parallel, eliminating
/// the global bottleneck while still preventing duplicate `UdpEndpoint::new`
/// calls for the same address (which would waste system resources on ephemeral
/// port exhaustion).
pub struct UdpEndpointPool {
    inner: Mutex<LruCache<SocketAddr, UdpEndpoint>>,
    /// Tracks which addresses currently have an in-flight `UdpEndpoint::new`.
    /// Insertion returns `true` iff the caller is the designated creator;
    /// other callers wait on the per-address [`Notify`] stored in
    /// [`notify_map`](Self::notify_map).
    creating: DashSet<SocketAddr>,
    /// Maps addresses being created to a [`Notify`] that will be signalled
    /// when creation completes (successfully or otherwise).
    notify_map: DashMap<SocketAddr, Arc<Notify>>,
}

impl UdpEndpointPool {
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size)
                    .expect("UdpEndpointPool max_size must be > 0; verify MAX_UDP_ENDPOINTS in consts"),
            )),
            creating: DashSet::new(),
            notify_map: DashMap::new(),
        }
    }

    /// Fast path: grab the socket of an existing (non-expired) endpoint.
    /// Returns `None` if no valid entry exists, `Some(())` if it exists.
    ///
    /// This avoids any String/Arc<str> cloning — the caller already has the
    /// target address from the UnderlaySession and can use it for `send_to`.
    /// Only the socket `try_clone()` syscall is performed.
    pub async fn get_socket(&self, addr: &SocketAddr) -> Option<std::net::UdpSocket> {
        let mut inner = self.inner.lock().await;
        let endpoint = inner.get_mut(addr)?;
        if endpoint.is_expired() {
            return None;
        }
        endpoint.touch();
        endpoint.socket.try_clone().ok()
    }

    /// Get or create a UDP endpoint for the given address.
    ///
    /// Returns `(socket, dial_target, is_new)` where `dial_target` is an
    /// `Arc<str>` (cloning it is just a refcount increment).
    ///
    /// # Concurrency design
    ///
    /// 1. **Fast path**: check the cache (`inner` lock, released immediately).
    /// 2. **Per-addr creation lock**: attempt to insert `addr` into the
    ///    [`creating`](Self::creating) set.  If another task is already
    ///    creating for this address, we register a per-addr [`Notify`] and
    ///    wait — without blocking creation for *other* addresses.
    /// 3. **Creation**: the designated creator calls `UdpEndpoint::new`,
    ///    inserts the result into the cache, removes the address from
    ///    `creating`, and notifies any waiters.
    /// 4. **Post-wakeup**: waiters retry the cache lookup.
    pub async fn get_or_create(
        &self,
        addr: SocketAddr,
        options: UdpEndpointOptions,
    ) -> anyhow::Result<((std::net::UdpSocket, Arc<str>), bool)> {
        // Use a loop instead of recursion to avoid infinitely sized futures
        // (Rust does not allow recursive async fn calls without boxing).
        loop {
            // ── Fast path: check cache without any creation lock. ──
            {
                let mut inner = self.inner.lock().await;
                if let Some(endpoint) = inner.get(&addr) {
                    if !endpoint.is_expired() {
                        let socket = endpoint.socket.try_clone()?;
                        let dial_target = endpoint.dial_target.clone();
                        return Ok(((socket, dial_target), false));
                    }
                }
            }

            // ── Per-addr creation arbitration via DashSet ──
            // Try to become the designated creator for this address.
            // We pre-allocate the Notify so that waiters can always find one.
            let notify = Arc::new(Notify::new());

            if self.creating.insert(addr) {
                // ── We are the creator ──
                // Register our Notify so that concurrent waiters can subscribe.
                self.notify_map.insert(addr, notify.clone());

                // Double-check: while we waited for the DashSet insert, another
                // task may have inserted a fresh endpoint into the cache.
                {
                    let mut inner = self.inner.lock().await;
                    if let Some(endpoint) = inner.get_mut(&addr) {
                        if !endpoint.is_expired() {
                            endpoint.touch();
                            let socket = endpoint.socket.try_clone()?;
                            let dial_target = endpoint.dial_target.clone();
                            // Clean up creation tracking before returning.
                            self.creating.remove(&addr);
                            self.notify_map.remove(&addr);
                            // Notify any concurrent waiters so they find the
                            // cached entry immediately.
                            notify.notify_waiters();
                            return Ok(((socket, dial_target), false));
                        }
                    }
                }

                // Confirmed: no valid endpoint exists, safely create one.
                let result = UdpEndpoint::new(options).await;

                match result {
                    Ok(endpoint) => {
                        let dial_target = endpoint.dial_target.clone();
                        let socket = endpoint.socket.try_clone()?;

                        let mut inner = self.inner.lock().await;
                        inner.put(addr, endpoint);

                        // Clean up creation tracking and notify waiters.
                        self.creating.remove(&addr);
                        self.notify_map.remove(&addr);
                        notify.notify_waiters();

                        return Ok(((socket, dial_target), true));
                    }
                    Err(e) => {
                        // Creation failed — clean up and notify waiters so they
                        // can retry or propagate the error.
                        self.creating.remove(&addr);
                        self.notify_map.remove(&addr);
                        notify.notify_waiters();
                        return Err(e);
                    }
                }
            }

            // ── Someone else is creating this endpoint — wait for them. ──
            // Register our interest in the notification map.
            // If the creator has already finished (unlikely but possible in a
            // race), the Notify will have been signalled and `notified().await`
            // will return immediately.
            let wait_notify = self
                .notify_map
                .entry(addr)
                .or_insert_with(|| Arc::new(Notify::new()))
                .value()
                .clone();

            // Double-check: the creator might have completed between our
            // `creating.insert` returning false and registering above.
            if !self.creating.contains(&addr) {
                // Creator finished — check the cache directly and retry
                // from the top of the loop.
                let mut inner = self.inner.lock().await;
                if let Some(endpoint) = inner.get_mut(&addr) {
                    if !endpoint.is_expired() {
                        endpoint.touch();
                        let socket = endpoint.socket.try_clone()?;
                        let dial_target = endpoint.dial_target.clone();
                        return Ok(((socket, dial_target), false));
                    }
                }
                // Cache miss (e.g. creation failed) — loop back to retry.
                continue;
            }

            // Wait for the creator to finish.
            wait_notify.notified().await;

            // Re-check the cache after wakeup.
            let mut inner = self.inner.lock().await;
            if let Some(endpoint) = inner.get_mut(&addr) {
                if !endpoint.is_expired() {
                    endpoint.touch();
                    let socket = endpoint.socket.try_clone()?;
                    let dial_target = endpoint.dial_target.clone();
                    return Ok(((socket, dial_target), false));
                }
            }

            // Something unexpected happened (creation failed and the Notify
            // was still signalled).  Loop back to retry from the top.
            continue;
        }
    }

    /// Remove a UDP endpoint from the cache by address.
    ///
    /// Typically called when a connection is closed or a send error is
    /// detected, ensuring stale endpoints do not linger in the pool.
    ///
    /// # Arguments
    ///
    /// * `addr` - The remote peer address whose endpoint should be removed.
    ///
    /// # Lock behaviour
    ///
    /// Acquires the inner [`tokio::sync::Mutex`] protecting the LRU cache
    /// briefly while performing the `pop` operation.  Other concurrent
    /// cache operations (`get_socket`, `get_or_create`) will wait for this
    /// lock to be released.
    pub async fn remove(&self, addr: &SocketAddr) {
        let mut inner = self.inner.lock().await;
        inner.pop(addr);
    }

    /// Clean up expired UDP endpoints from the pool.
    ///
    /// Iterates all entries in the internal LRU cache, collects expired
    /// endpoints (those whose idle time exceeds `nat_timeout`), and removes
    /// them.  Intended to be invoked periodically from a background async
    /// task (e.g. via `tokio::time::interval`).
    ///
    /// # Lock behaviour
    ///
    /// Acquires the inner [`tokio::sync::Mutex`] for the full duration of
    /// the sweep.  While this blocks concurrent `get_socket` / `get_or_create`
    /// / `remove` calls, the sweep is designed to complete quickly
    /// (O(n) in the number of cached endpoints, typically a few hundred).
    ///
    /// # Arguments
    ///
    /// * `self` - Shared reference to the pool.
    pub async fn cleanup_async(&self) {
        let mut inner = self.inner.lock().await;
        // LruCache does not have retain(), so collect expired keys first.
        let expired: Vec<SocketAddr> = inner
            .iter()
            .filter(|(_, endpoint)| endpoint.is_expired())
            .map(|(addr, _)| *addr)
            .collect();
        for addr in expired {
            inner.pop(&addr);
        }
    }
}
