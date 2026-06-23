use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use lru::LruCache;
use tokio::sync::Mutex;

/// Options for creating a UDP endpoint
pub struct UdpEndpointOptions {
    pub nat_timeout: Duration,
    pub dial_target: String,
}

/// A UDP endpoint representing a full-cone NAT session
pub struct UdpEndpoint {
    pub socket: std::net::UdpSocket,
    pub dial_target: String,
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
            dial_target: options.dial_target,
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
pub struct UdpEndpointPool {
    inner: Mutex<LruCache<SocketAddr, UdpEndpoint>>,
    /// Serializes UdpEndpoint::new calls to prevent TOCTOU race and
    /// mitigate temporary port exhaustion under high concurrency.
    create_lock: Mutex<()>,
}

impl UdpEndpointPool {
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(NonZeroUsize::new(max_size).unwrap())),
            create_lock: Mutex::new(()),
        }
    }

    pub async fn get_or_create(
        &self,
        addr: SocketAddr,
        options: UdpEndpointOptions,
    ) -> anyhow::Result<((std::net::UdpSocket, String), bool)> {
        // Fast path: check if already exists without acquiring the create lock.
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

        // Acquire the create lock to serialize UdpEndpoint::new calls,
        // so that at most one task creates a socket for a given addr.
        let _create_guard = self.create_lock.lock().await;

        // Double-check: while we waited for the create lock, another task
        // may have already inserted a fresh endpoint.
        {
            let mut inner = self.inner.lock().await;
            if let Some(endpoint) = inner.get_mut(&addr) {
                if !endpoint.is_expired() {
                    endpoint.touch();
                    let socket = endpoint.socket.try_clone()?;
                    let dial_target = endpoint.dial_target.clone();
                    return Ok(((socket, dial_target), false));
                }
            }
        }

        // Confirmed: no valid endpoint exists, safely create one.
        let endpoint = UdpEndpoint::new(options).await?;

        let mut inner = self.inner.lock().await;
        let dial_target = endpoint.dial_target.clone();
        let socket = endpoint.socket.try_clone()?;
        inner.put(addr, endpoint);

        Ok(((socket, dial_target), true))
    }

    pub async fn remove(&self, addr: &SocketAddr) {
        let mut inner = self.inner.lock().await;
        inner.pop(addr);
    }

    /// Clean up expired endpoints. Called from a periodic async task.
    /// Uses the async mutex directly since it runs in an async context.
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
