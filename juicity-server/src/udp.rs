use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

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
    created_at: Instant,
    nat_timeout: Duration,
}

impl UdpEndpoint {
    /// Create a new UDP endpoint bound to a random port.
    /// Uses tokio::net::UdpSocket for async bind to avoid blocking the runtime.
    pub async fn new(options: UdpEndpointOptions) -> anyhow::Result<Self> {
        let tokio_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let socket = tokio_socket.into_std()?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket,
            dial_target: options.dial_target,
            created_at: Instant::now(),
            nat_timeout: options.nat_timeout,
        })
    }

    pub fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.created_at) > self.nat_timeout
    }

    pub fn touch(&mut self) {
        self.created_at = Instant::now();
    }
}

/// Pool of UDP endpoints for full-cone NAT
pub struct UdpEndpointPool {
    inner: Mutex<HashMap<SocketAddr, UdpEndpoint>>,
}

impl UdpEndpointPool {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_create(
        &self,
        addr: SocketAddr,
        options: UdpEndpointOptions,
    ) -> anyhow::Result<((std::net::UdpSocket, String), bool)> {
        // Fast path: check if already exists without creating a new endpoint.
        // We hold the lock only briefly to check and clone.
        {
            let mut inner = self.inner.lock().await;
            if let Some(endpoint) = inner.get_mut(&addr) {
                if !endpoint.is_expired() {
                    endpoint.touch();
                    // Clone socket and dial_target only when needed (existing session)
                    let socket = endpoint.socket.try_clone()?;
                    let dial_target = endpoint.dial_target.clone();
                    return Ok(((socket, dial_target), false));
                }
            }
        }

        // Slow path: create a new endpoint outside the lock to avoid holding
        // the mutex across an async bind (which would block other tasks).
        let endpoint = UdpEndpoint::new(options).await?;

        let mut inner = self.inner.lock().await;
        // Re-check: a concurrent task may have raced through the slow path and
        // already inserted a fresh entry while we were binding. Prefer the existing
        // entry to avoid leaking the socket we just created.
        if let Some(existing) = inner.get_mut(&addr) {
            if !existing.is_expired() {
                existing.touch();
                let socket = existing.socket.try_clone()?;
                let dial_target = existing.dial_target.clone();
                // Explicitly drop the unused endpoint to release its kernel port binding
                // immediately, preventing port exhaustion under high concurrency.
                drop(endpoint);
                return Ok(((socket, dial_target), false));
            }
        }
        let dial_target = endpoint.dial_target.clone();
        let socket = endpoint.socket.try_clone()?;
        inner.insert(addr, endpoint);

        Ok(((socket, dial_target), true))
    }

    pub async fn remove(&self, addr: &SocketAddr) {
        let mut inner = self.inner.lock().await;
        inner.remove(addr);
    }

    /// Clean up expired endpoints. Called from a periodic async task.
    /// Uses the async mutex directly since it runs in an async context.
    pub async fn cleanup_async(&self) {
        let mut inner = self.inner.lock().await;
        inner.retain(|_, endpoint| !endpoint.is_expired());
    }

    /// Sync cleanup with try_lock (kept for backward compatibility).
    /// Prefer cleanup_async() in async contexts.
    pub fn cleanup(&self) {
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.retain(|_, endpoint| !endpoint.is_expired());
        }
    }
}
