use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::{DashMap, DashSet};
use lru::LruCache;
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
///
/// # Lock-free inner cache
///
/// The inner [`Mutex<LruCache>`] uses [`std::sync::Mutex`] instead of
/// [`tokio::sync::Mutex`] because all critical sections are sub-microsecond
/// (LRU get/put/remove) and no `.await` point is ever held under the lock.
/// Using `std::sync::Mutex` avoids the additional bookkeeping overhead of
/// Tokio's async mutex for these extremely short operations.
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
            inner: Mutex::new(LruCache::new(NonZeroUsize::new(max_size).expect(
                "UdpEndpointPool max_size must be > 0; verify MAX_UDP_ENDPOINTS in consts",
            ))),
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
    ///
    /// Uses `std::sync::Mutex` — lock is held only for the duration of the
    /// lookup, touch, and clone operations. No `.await` is held under the lock.
    pub fn get_socket(&self, addr: &SocketAddr) -> Option<std::net::UdpSocket> {
        let mut inner = self.inner.lock().ok()?;
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
    /// 3. **Creation**: the designated caller calls `UdpEndpoint::new`,
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
                let mut inner = self
                    .inner
                    .lock()
                    .map_err(|e| anyhow::anyhow!("mutex poisoned: {:?}", e))?;
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
                    let mut inner = self
                        .inner
                        .lock()
                        .map_err(|e| anyhow::anyhow!("mutex poisoned: {:?}", e))?;
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

                        let mut inner = self
                            .inner
                            .lock()
                            .map_err(|e| anyhow::anyhow!("mutex poisoned: {:?}", e))?;
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
                let mut inner = self
                    .inner
                    .lock()
                    .map_err(|e| anyhow::anyhow!("mutex poisoned: {:?}", e))?;
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
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| anyhow::anyhow!("mutex poisoned: {:?}", e))?;
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
    pub fn remove(&self, addr: &SocketAddr) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.pop(addr);
        }
    }

    /// Clean up expired UDP endpoints from the pool.
    ///
    /// Iterates all entries in the internal LRU cache, collects expired
    /// endpoints (those whose idle time exceeds `nat_timeout`), and removes
    /// them.  Intended to be invoked periodically from a background tokio
    /// task (e.g. via `tokio::time::interval`).
    ///
    /// # Lock behaviour
    ///
    /// Uses a two-phase approach: first collects expired keys under the
    /// inner [`std::sync::Mutex`], then releases the lock and removes each
    /// expired entry with a separate, short lock acquisition.  This gives
    /// concurrent `get_socket` / `get_or_create` / `remove` calls a chance
    /// to proceed between individual removals, reducing latency spikes
    /// under contention.  The initial collection is O(n) in the number of
    /// cached endpoints (typically a few hundred).
    ///
    /// # Arguments
    ///
    /// * `self` - Shared reference to the pool.
    pub fn cleanup(&self) {
        // Phase 1: collect expired keys under lock.
        let expired: Vec<SocketAddr> = {
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            inner
                .iter()
                .filter(|(_, endpoint)| endpoint.is_expired())
                .map(|(addr, _)| *addr)
                .collect()
        }; // lock is released here

        // Phase 2: re-check expiry then remove with separate, short lock acquisitions.
        // A re-check is necessary because between Phase 1 and Phase 2 a concurrent
        // get_socket / get_or_create / remove call may have touched, replaced, or
        // deleted the endpoint for this address.  Removing blindly here could:
        //   (a) delete a freshly-created endpoint (TOCTOU race), or
        //   (b) cause a double-remove if another task already removed it.
        // Both are harmless with the re-check: we only pop if the entry still
        // exists AND is still expired at this moment.
        //
        // We use peek() instead of get() to avoid bumping the LRU order of an
        // entry we are about to remove.
        //
        // This approach preserves the original design goal of releasing the lock
        // between individual removals, so concurrent access to unrelated addresses
        // is not blocked during a full sweep.
        for addr in expired {
            if let Ok(mut inner) = self.inner.lock() {
                let still_expired = inner.peek(&addr).map_or(false, |e| e.is_expired());
                if still_expired {
                    inner.pop(&addr);
                }
            }
        }
    }
}
