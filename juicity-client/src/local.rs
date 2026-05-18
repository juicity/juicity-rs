use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use juicity_common::consts;
use juicity_common::protocol;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};

use crate::client::JuicityClient;

/// Local proxy server that handles SOCKS5 and HTTP proxy
pub struct LocalServer {
    bind_addr: String,
    client: JuicityClient,
}

#[derive(Clone)]
struct UdpOutboundDatagram {
    addr: String,
    port: u16,
    payload: Bytes,
}

struct UdpSessionEntry {
    id: u64,
    tx: mpsc::Sender<UdpOutboundDatagram>,
}

impl LocalServer {
    pub fn new(bind_addr: String, client: JuicityClient) -> Self {
        Self { bind_addr, client }
    }

    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        tracing::info!("Local proxy listening on {}", self.bind_addr);

        loop {
            let (stream, addr) = listener.accept().await?;
            let client = self.client.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, addr, client).await {
                    tracing::debug!("Connection handler error: {:?}", e);
                }
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    _addr: SocketAddr,
    client: JuicityClient,
) -> anyhow::Result<()> {
    let mut buf = [0u8; 1];
    stream.peek(&mut buf).await?;

    match buf[0] {
        0x05 => handle_socks5(stream, client).await,
        _ => handle_http_proxy(stream, client).await,
    }
}

/// Handle a SOCKS5 proxy connection
async fn handle_socks5(mut stream: TcpStream, client: JuicityClient) -> anyhow::Result<()> {
    // Handshake: read methods
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    let n_methods = buf[1] as usize;
    let mut methods = vec![0u8; n_methods];
    stream.read_exact(&mut methods).await?;
    // Accept no-auth
    stream.write_all(&[0x05, 0x00]).await?;

    // Read request
    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await?;
    let _ver = req[0];
    let cmd = req[1]; // 1=CONNECT, 3=UDP ASSOCIATE
    let _rsv = req[2];
    let addr_type = req[3];

    let (host, port) = match addr_type {
        0x01 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            (
                std::net::Ipv4Addr::from(ip).to_string(),
                u16::from_be_bytes(port_buf),
            )
        }
        0x03 => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            stream.read_exact(&mut domain).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            (
                String::from_utf8(domain)?,
                u16::from_be_bytes(port_buf),
            )
        }
        0x04 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            (
                std::net::Ipv6Addr::from(ip).to_string(),
                u16::from_be_bytes(port_buf),
            )
        }
        _ => anyhow::bail!("unsupported address type: {}", addr_type),
    };

    match cmd {
        0x01 => {
            // TCP CONNECT
            tracing::debug!("SOCKS5 CONNECT: {}:{}", host, port);
            let response = build_socks5_response(0x00, &host, port);
            stream.write_all(&response).await?;

            let (mut quic_send, mut quic_recv) = client.open_tcp_stream(&host, port).await?;

            let (mut local_rx, mut local_tx) = stream.split();
            tokio::select! {
                r = tokio::io::copy(&mut local_rx, &mut quic_send) => {
                    if let Err(e) = r {
                        tracing::debug!("SOCKS5 local->quic: {:?}", e);
                    }
                }
                r = tokio::io::copy(&mut quic_recv, &mut local_tx) => {
                    if let Err(e) = r {
                        tracing::debug!("SOCKS5 quic->local: {:?}", e);
                    }
                }
            }
        }
        0x03 => {
            // UDP ASSOCIATE
            tracing::debug!("SOCKS5 UDP ASSOCIATE: {}:{}", host, port);

            // Bind a local UDP port for the SOCKS5 client to send UDP datagrams to
            let bind_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
            let udp_listen_addr = bind_socket.local_addr()?;

            // Send success response with the actual UDP listening address
            let response = build_socks5_response(
                0x00,
                &udp_listen_addr.ip().to_string(),
                udp_listen_addr.port(),
            );
            stream.write_all(&response).await?;

            let client_clone = client.clone();
            let bind_socket_clone = bind_socket.clone();
            let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let session_seq = Arc::new(AtomicU64::new(1));

            // Spawn UDP forwarder. Per Juicity spec, datagrams from the same source
            // address triplet SHOULD share one stream and be recycled by NAT timeout.
            tokio::spawn(async move {
                let mut buf = vec![0u8; consts::ETHERNET_MTU];
                loop {
                    tokio::select! {
                        result = bind_socket_clone.recv_from(&mut buf) => {
                            match result {
                                Ok((n, src)) => {
                                    let datagram = match parse_socks5_udp_request(&buf[..n]) {
                                        Some(v) => v,
                                        None => continue,
                                    };

                                    let existing = {
                                        let guard = sessions.lock().await;
                                        guard.get(&src).map(|s| (s.id, s.tx.clone()))
                                    };

                                    if let Some((session_id, tx)) = existing {
                                        if tx.send(datagram.clone()).await.is_ok() {
                                            continue;
                                        }
                                        remove_session_if_match(&sessions, src, session_id).await;
                                    }

                                    let new_session_id = session_seq.fetch_add(1, Ordering::Relaxed);
                                    match start_udp_assoc_session(
                                        client_clone.clone(),
                                        bind_socket_clone.clone(),
                                        sessions.clone(),
                                        src,
                                        new_session_id,
                                        datagram,
                                    )
                                    .await
                                    {
                                        Ok(tx) => {
                                            let mut guard = sessions.lock().await;
                                            guard.insert(
                                                src,
                                                UdpSessionEntry {
                                                    id: new_session_id,
                                                    tx,
                                                },
                                            );
                                        }
                                        Err(e) => {
                                            tracing::debug!("UDP ASSOCIATE session open error: {:?}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("UDP read error: {:?}", e);
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(consts::DEFAULT_NAT_TIMEOUT) => {
                            // NAT timeout — clean up all sessions before breaking
                            let mut guard = sessions.lock().await;
                            guard.clear();
                            break;
                        }
                    }
                }
            });

            // Keep the TCP control connection alive until the client disconnects
            let mut dummy = [0u8; 1];
            let _ = stream.read(&mut dummy).await;
        }
        _ => {
            let response = build_socks5_response(0x07, "0.0.0.0", 0);
            stream.write_all(&response).await?;
        }
    }

    Ok(())
}

async fn start_udp_assoc_session(
    client: JuicityClient,
    bind_socket: Arc<UdpSocket>,
    sessions: Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>>,
    local_client_addr: SocketAddr,
    session_id: u64,
    first_datagram: UdpOutboundDatagram,
) -> anyhow::Result<mpsc::Sender<UdpOutboundDatagram>> {
    let (mut send, mut recv) = client
        .open_udp_stream(
            &first_datagram.addr,
            first_datagram.port,
            &first_datagram.payload[..],
        )
        .await?;

    let (tx, mut rx) = mpsc::channel::<UdpOutboundDatagram>(256);

    let sessions_for_supervisor = sessions.clone();
    let bind_socket_for_reader = bind_socket.clone();

    tokio::spawn(async move {
        let mut writer = tokio::spawn(async move {
            loop {
                match tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, rx.recv()).await {
                    Ok(Some(datagram)) => {
                        if JuicityClient::send_udp_datagram(
                            &mut send,
                            &datagram.addr,
                            datagram.port,
                            &datagram.payload[..],
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = send.finish();
        });

        let mut reader = tokio::spawn(async move {
            loop {
                let (resp_addr, resp_port, resp_payload) = match read_one_udp_response(&mut recv).await {
                    Ok(v) => v,
                    Err(_) => break,
                };

                let socks5_packet = build_socks5_udp_packet(&resp_addr, resp_port, &resp_payload);
                if bind_socket_for_reader
                    .send_to(&socks5_packet, local_client_addr)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        tokio::select! {
            _ = &mut writer => {
                reader.abort();
                let _ = reader.await;
            }
            _ = &mut reader => {
                writer.abort();
                let _ = writer.await;
            }
        }

        remove_session_if_match(&sessions_for_supervisor, local_client_addr, session_id).await;
    });

    Ok(tx)
}

async fn read_one_udp_response(
    recv: &mut quinn::RecvStream,
) -> anyhow::Result<(String, u16, Vec<u8>)> {
    // Wire format (upstream-compatible): [trojanc_addr][len(2)][payload]
    let (resp_addr, resp_port) = tokio::time::timeout(
        consts::DEFAULT_NAT_TIMEOUT,
        protocol::read_trojanc_addr_async(recv),
    )
    .await??;

    let mut len_buf = [0u8; 2];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut len_buf)).await??;
    let pkt_len = u16::from_be_bytes(len_buf) as usize;
    let mut resp_payload = vec![0u8; pkt_len];
    tokio::time::timeout(consts::DEFAULT_NAT_TIMEOUT, recv.read_exact(&mut resp_payload))
        .await??;

    Ok((resp_addr, resp_port, resp_payload))
}

async fn remove_session_if_match(
    sessions: &Arc<Mutex<HashMap<SocketAddr, UdpSessionEntry>>>,
    local_client_addr: SocketAddr,
    session_id: u64,
) {
    let mut guard = sessions.lock().await;
    if let Some(existing) = guard.get(&local_client_addr) {
        if existing.id == session_id {
            guard.remove(&local_client_addr);
        }
    }
}

fn parse_socks5_udp_request(packet: &[u8]) -> Option<UdpOutboundDatagram> {
    // SOCKS5 UDP request: RSV(2) + FRAG(1) + ATYP(1) + DST.ADDR + DST.PORT(2) + DATA
    if packet.len() < 4 {
        return None;
    }

    // Fragmented UDP is not supported.
    if packet[2] != 0x00 {
        return None;
    }

    let mut offset = 3usize;
    let atyp = *packet.get(offset)?;
    offset += 1;

    let (addr, port) = match atyp {
        0x01 => {
            if packet.len() < offset + 4 + 2 {
                return None;
            }
            let ip = std::net::Ipv4Addr::new(
                packet[offset],
                packet[offset + 1],
                packet[offset + 2],
                packet[offset + 3],
            );
            offset += 4;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (ip.to_string(), p)
        }
        0x03 => {
            let dlen = *packet.get(offset)? as usize;
            offset += 1;
            if packet.len() < offset + dlen + 2 {
                return None;
            }
            let domain = String::from_utf8(packet[offset..offset + dlen].to_vec()).ok()?;
            offset += dlen;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (domain, p)
        }
        0x04 => {
            if packet.len() < offset + 16 + 2 {
                return None;
            }
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&packet[offset..offset + 16]);
            offset += 16;
            let p = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
            offset += 2;
            (std::net::Ipv6Addr::from(ip).to_string(), p)
        }
        _ => return None,
    };

    if packet.len() < offset {
        return None;
    }

    Some(UdpOutboundDatagram {
        addr,
        port,
        payload: Bytes::copy_from_slice(&packet[offset..]),
    })
}

fn build_socks5_udp_packet(addr: &str, port: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(payload.len() + 32);
    packet.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV, RSV, FRAG

    if let Ok(ipv4) = addr.parse::<std::net::Ipv4Addr>() {
        packet.push(0x01);
        packet.extend_from_slice(&ipv4.octets());
    } else if let Ok(ipv6) = addr.parse::<std::net::Ipv6Addr>() {
        packet.push(0x04);
        packet.extend_from_slice(&ipv6.octets());
    } else {
        let domain_bytes = addr.as_bytes();
        packet.push(0x03);
        packet.push(domain_bytes.len() as u8);
        packet.extend_from_slice(domain_bytes);
    }

    packet.extend_from_slice(&port.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// Handle HTTP CONNECT proxy
async fn handle_http_proxy(mut stream: TcpStream, client: JuicityClient) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (reader, mut writer) = stream.split();
    let mut buf_reader = BufReader::new(reader);
    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line).await?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        anyhow::bail!("invalid HTTP request: {}", request_line.trim());
    }

    let method = parts[0];
    let target = parts[1];

    match method {
        "CONNECT" => {
            // Parse host:port from the CONNECT target.
            // Use rsplitn(2, ':') to correctly handle IPv6 addresses like [::1]:443.
            let (host, port) = match target.rsplitn(2, ':').collect::<Vec<&str>>() {
                parts if parts.len() == 2 => {
                    let port = parts[0].parse::<u16>().unwrap_or(443);
                    let host = parts[1].to_string();
                    (host, port)
                }
                _ => (target.to_string(), 443u16),
            };

            tracing::debug!("HTTP CONNECT: {}:{}", host, port);

            loop {
                let mut line = String::new();
                buf_reader.read_line(&mut line).await?;
                if line.trim().is_empty() {
                    break;
                }
            }

            writer
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;

            let mut stream = buf_reader.into_inner();
            let (mut quic_send, mut quic_recv) = client.open_tcp_stream(&host, port).await?;

            tokio::select! {
                r = tokio::io::copy(&mut stream, &mut quic_send) => {
                    if let Err(e) = r {
                        tracing::debug!("HTTP CONNECT local->quic: {:?}", e);
                    }
                }
                r = tokio::io::copy(&mut quic_recv, &mut writer) => {
                    if let Err(e) = r {
                        tracing::debug!("HTTP CONNECT quic->local: {:?}", e);
                    }
                }
            }
        }
        _ => {
            writer
                .write_all(b"HTTP/1.1 501 Not Implemented\r\n\r\n")
                .await?;
        }
    }

    Ok(())
}

/// Build a SOCKS5 response, automatically detecting the address type from the host string.
fn build_socks5_response(reply: u8, host: &str, port: u16) -> Vec<u8> {
    let (addr_type, addr_bytes): (u8, Vec<u8>) = if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        (0x01, ip.octets().to_vec())
    } else if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        (0x04, ip.octets().to_vec())
    } else {
        let domain_bytes = host.as_bytes();
        let mut bytes = Vec::with_capacity(1 + domain_bytes.len());
        bytes.push(domain_bytes.len() as u8);
        bytes.extend_from_slice(domain_bytes);
        (0x03, bytes)
    };

    let mut response = Vec::with_capacity(4 + addr_bytes.len() + 2);
    response.extend_from_slice(&[0x05, reply, 0x00, addr_type]);
    response.extend_from_slice(&addr_bytes);
    response.extend_from_slice(&port.to_be_bytes());
    response
}
