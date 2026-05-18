use tokio::net::{TcpStream, UdpSocket};

/// A trait for dialing outbound TCP/UDP connections
#[async_trait::async_trait]
pub trait Dialer: Send + Sync {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream>;
    async fn dial_udp(&self, addr: &str) -> anyhow::Result<UdpSocket>;
}

/// Default dialer that connects directly
pub struct DefaultDialer;

#[async_trait::async_trait]
impl Dialer for DefaultDialer {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    async fn dial_udp(&self, _addr: &str) -> anyhow::Result<UdpSocket> {
        // The addr parameter is the target address we'll send packets to.
        // We bind to a local ephemeral port without connecting, so the caller
        // can use send_to() to send packets to different targets if needed.
        // Pre-connecting via socket.connect(addr) would restrict us to a single target.
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(socket)
    }
}

/// Dialer that binds to a specific IP address
pub struct BindDialer {
    pub bind_addr: std::net::IpAddr,
}

#[async_trait::async_trait]
impl Dialer for BindDialer {
    async fn dial_tcp(&self, addr: &str) -> anyhow::Result<TcpStream> {
        // Use tokio's TcpSocket for binding
        // Select IPv4 or IPv6 based on bind_addr address family
        let socket = if self.bind_addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        socket.bind(std::net::SocketAddr::new(self.bind_addr, 0))?;
        let stream = socket.connect(addr.parse()?).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    async fn dial_udp(&self, _addr: &str) -> anyhow::Result<UdpSocket> {
        // The addr parameter is the target address we'll send packets to.
        // We bind to a local ephemeral port without connecting, so the caller
        // can use send_to() to send packets to different targets if needed.
        let bind_addr = std::net::SocketAddr::new(self.bind_addr, 0);
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(socket)
    }
}
