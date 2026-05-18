use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Options for creating a UDP endpoint
pub struct UdpEndpointOptions {
    pub handler: Box<dyn Fn(&[u8], SocketAddr) -> anyhow::Result<()> + Send + Sync>,
    pub nat_timeout: Duration,
    pub dial_target: String,
}

/// A UDP endpoint representing a full-cone NAT session
pub struct UdpEndpoint {
    pub socket: std::net::UdpSocket,
    pub dial_target: String,
    created_at: Instant,
    nat_timeout: Duration,
    // handler is kept for API compatibility but unused internally
    #[allow(dead_code)]
    handler: Box<dyn Fn(&[u8], SocketAddr) -> anyhow::Result<()> + Send + Sync>,
}

impl UdpEndpoint {
    pub fn new(options: UdpEndpointOptions) -> anyhow::Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            socket,
            dial_target: options.dial_target,
            created_at: Instant::now(),
            nat_timeout: options.nat_timeout,
            handler: options.handler,
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
        let mut inner = self.inner.lock().await;

        // Check if already exists
        if let Some(endpoint) = inner.get_mut(&addr) {
            if !endpoint.is_expired() {
                endpoint.touch();
                return Ok((
                    (endpoint.socket.try_clone()?, endpoint.dial_target.clone()),
                    false,
                ));
            }
        }

        let endpoint = UdpEndpoint::new(options)?;
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
