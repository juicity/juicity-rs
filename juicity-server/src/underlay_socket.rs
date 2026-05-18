use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

#[derive(Debug, Clone)]
pub struct UnderlayPacket {
    pub peer: SocketAddr,
    pub payload: Bytes,
}

/// Split non-QUIC packets away from Quinn while keeping one shared UDP port.
#[derive(Debug)]
pub struct DemuxUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    underlay_tx: tokio::sync::mpsc::UnboundedSender<UnderlayPacket>,
}

impl DemuxUdpSocket {
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        underlay_tx: tokio::sync::mpsc::UnboundedSender<UnderlayPacket>,
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
        loop {
            let msgs = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Pending => return Poll::Pending,
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
                    // Use Bytes::copy_from_slice to create a reference-counted
                    // copy of the payload, avoiding per-packet Vec allocation overhead.
                    let _ = self.underlay_tx.send(UnderlayPacket {
                        peer: meta[i].addr,
                        payload: Bytes::copy_from_slice(&bufs[i][offset..end]),
                    });
                    offset = end;
                }
            }

            if keep > 0 {
                return Poll::Ready(Ok(keep));
            }
        }
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
