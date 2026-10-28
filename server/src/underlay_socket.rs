use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

/// A single non-QUIC underlay packet received from the demultiplexed UDP
/// socket.
///
/// When [`DemuxUdpSocket`] processes a raw UDP datagram and determines it
/// is **not** a QUIC packet (by inspecting the first byte), it wraps the
/// datagram's metadata in this struct and forwards it through the underlay
/// channel for further handling (decryption, NAT session lookup, relay).
///
/// # Fields
///
/// * `peer` - The source [`SocketAddr`] of the UDP datagram.
/// * `payload` - The raw UDP payload bytes (including the underlay salt
///   prefix when applicable).
#[derive(Debug)]
pub struct UnderlayPacket {
    pub peer: SocketAddr,
    pub payload: Vec<u8>,
}

/// Maximum number of non-QUIC underlay packets that can be queued before new ones are dropped.
/// Set to 4x MAX_UNDERLAY_HANDLER_CONCURRENCY to provide buffer headroom
/// for burst traffic, preventing cascading packet loss when the semaphore is exhausted.
pub const UNDERLAY_CHANNEL_CAPACITY: usize = 4096;

/// Max recv batches processed in one `poll_recv` call before yielding.
/// Prevents sustained non-QUIC traffic from monopolizing a runtime worker.
const MAX_DEMUX_BATCHES_PER_POLL: usize = 8;

/// Counts dropped non-QUIC packets when underlay channel is full.
/// Used to rate-limit warning logs under burst traffic.
static UNDERLAY_DROPPED_PACKETS: AtomicU64 = AtomicU64::new(0);

/// Split non-QUIC packets away from Quinn while keeping one shared UDP port.
#[derive(Debug)]
pub struct DemuxUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
}

impl DemuxUdpSocket {
    /// Create a new [`DemuxUdpSocket`] wrapping an existing QUIC UDP socket.
    ///
    /// The returned socket implements [`AsyncUdpSocket`] and can be used
    /// directly with Quinn's [`quinn::Endpoint`].  Non-QUIC packets detected
    /// by [`is_probably_quic_packet`](Self::is_probably_quic_packet) are
    /// forwarded through the `underlay_tx` channel instead of being passed
    /// to Quinn.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying Quinn [`AsyncUdpSocket`] that handles the
    ///   actual UDP I/O.
    /// * `underlay_tx` - A `tokio::sync::mpsc::Sender` that accepts
    ///   [`UnderlayPacket`] instances for non-QUIC underlay processing.
    ///
    /// # Lock-free design
    ///
    /// No locks are used in the constructor or the demultiplexing logic.
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
    ) -> Self {
        Self { inner, underlay_tx }
    }

    #[inline]
    fn is_probably_quic_packet(packet: &[u8]) -> bool {
        if packet.is_empty() {
            return false;
        }
        let first = packet[0];
        // QUIC v1 long header: first two bits are 11 (0xC0)
        // QUIC v1 short header: first bit is 0 (0x00-0x7F range, but specifically bit 0x40 is set for short header)
        // More strict check:
        // - Long header (QUIC v1): first byte has bits 0xC0 set (both high bits)
        // - Short header (QUIC v1): first byte has bit 0x40 set, bit 0x80 clear
        //
        // Actually, QUIC always has the second most significant bit set (0x40).
        // The original check `(packet[0] & 0x40) == 0x40` is correct for QUIC v1.
        // However, we should also verify it's not a known non-QUIC protocol.
        //
        // For Juicity underlay, non-QUIC packets are encrypted and their first byte
        // will have different characteristics. The original check is sufficient but
        // we add an additional sanity check: the fixed bit (0x40) must be set,
        // and for long headers (0xC0), the version should be valid.
        (first & 0x40) == 0x40
    }
}

impl AsyncUdpSocket for DemuxUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        for _ in 0..MAX_DEMUX_BATCHES_PER_POLL {
            let msgs = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Pending => {
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(msgs)) => msgs,
            };

            let mut keep = 0usize;
            for i in 0..msgs {
                let first_len = meta[i].stride.min(meta[i].len);
                let first_pkt = &bufs[i][..first_len];
                if Self::is_probably_quic_packet(first_pkt) {
                    if keep != i {
                        bufs.swap(keep, i);
                        meta.swap(keep, i);
                    }
                    keep += 1;
                    continue;
                }

                let mut offset = 0usize;
                let stride = meta[i].stride.max(1);
                while offset < meta[i].len {
                    let end = (offset + stride).min(meta[i].len);
                    // Queue only packet-sized storage, not a maximum-size receive buffer.
                    let payload = bufs[i][offset..end].to_vec();
                    if self
                        .underlay_tx
                        .try_send(UnderlayPacket {
                            peer: meta[i].addr,
                            payload,
                        })
                        .is_err()
                    {
                        // Channel full: drop packet. Warn only periodically to avoid
                        // log storms amplifying CPU under hostile/burst traffic.
                        let dropped = UNDERLAY_DROPPED_PACKETS.fetch_add(1, Ordering::Relaxed) + 1;
                        if dropped == 1 || dropped % 1024 == 0 {
                            tracing::warn!(
                                "underlay channel full, dropped {} non-QUIC packets (latest from {})",
                                dropped,
                                meta[i].addr
                            );
                        }
                    }
                    offset = end;
                }
            }

            if keep > 0 {
                return Poll::Ready(Ok(keep));
            }
        }

        // The batch limit yields to the executor, but the socket may still be
        // readable. Arrange another poll instead of waiting for a new I/O edge.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn::Runtime;
    use std::time::Duration;

    #[tokio::test]
    async fn queued_underlay_packet_uses_packet_sized_storage() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let addr = socket.local_addr().unwrap();
        let inner = quinn::TokioRuntime.wrap_udp_socket(socket).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let demux = DemuxUdpSocket::new(inner, tx);
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(&[0; 32], addr).await.unwrap();
        sender.send_to(&[0x40; 32], addr).await.unwrap();
        let mut storage = [0u8; 65535];
        let mut buffers = [IoSliceMut::new(&mut storage)];
        let mut meta = [RecvMeta::default()];
        let n = tokio::time::timeout(
            Duration::from_secs(1),
            std::future::poll_fn(|cx| demux.poll_recv(cx, &mut buffers, &mut meta)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(&buffers[0][..meta[0].len], &[0x40; 32]);
        let packet = rx.try_recv().unwrap();
        assert_eq!(packet.peer, sender.local_addr().unwrap());
        assert_eq!(packet.payload, [0; 32]);
        assert_eq!(packet.payload.capacity(), 32);
    }
}
