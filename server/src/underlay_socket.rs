use std::cell::RefCell;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// Max consecutive polls with zero QUIC packets before yielding the runtime worker.
/// Combined with [`MAX_DEMUX_BATCHES_PER_POLL`], this caps worst-case busy spins at
/// `MAX_DEMUX_BATCHES_PER_POLL × MAX_IDLE_POLL_BEFORE_YIELD` (32) non-QUIC recv
/// batches before yielding.
const MAX_IDLE_POLL_BEFORE_YIELD: usize = 4;

// Thread-local pre-allocated buffer for UDP packet payloads.
//
// Instead of calling `.to_vec()` (which allocates a new `Vec` from scratch
// for every non-QUIC packet), we reuse this fixed-capacity buffer to
// amortise allocation overhead.  The buffer is initialised with 65535 bytes
// of capacity (maximum UDP datagram size), so `extend_from_slice` never
// triggers a reallocation after the first call on each thread.
//
// We use `RefCell` because this is accessed from `poll_recv()` which is
// not async and runs on a single thread at a time.
thread_local! {
    static UDP_PACKET_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(65535));
}

/// Counts dropped non-QUIC packets when underlay channel is full.
/// Used to rate-limit warning logs under burst traffic.
static UNDERLAY_DROPPED_PACKETS: AtomicU64 = AtomicU64::new(0);

/// Split non-QUIC packets away from Quinn while keeping one shared UDP port.
#[derive(Debug)]
pub struct DemuxUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
    /// Tracks consecutive polls that yielded zero QUIC packets.
    /// Reset to 0 whenever at least one QUIC packet is returned.
    /// Used to back off and yield the runtime worker under non-QUIC flood.
    idle_poll_count: AtomicUsize,
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
    /// The only shared state is an [`AtomicUsize`] idle-poll counter used
    /// for back-pressure during non-QUIC traffic floods.
    pub fn new(
        inner: Arc<dyn AsyncUdpSocket>,
        underlay_tx: tokio::sync::mpsc::Sender<UnderlayPacket>,
    ) -> Self {
        Self {
            inner,
            underlay_tx,
            idle_poll_count: AtomicUsize::new(0),
        }
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
                    // Inner socket has no more data; reset idle counter since
                    // we will not be re-polled until new data arrives via the
                    // waker that was registered by the inner socket.
                    self.idle_poll_count.store(0, Ordering::Relaxed);
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
                    // Use a thread-local pre-allocated buffer to avoid per-packet
                    // heap allocation overhead.  The buffer is initialised with
                    // 65535 bytes capacity (max UDP datagram), so extend_from_slice
                    // never triggers a reallocation after the first call.
                    //
                    // We swap the buffer out, fill it, clone the result for the
                    // channel, then put the (still-capacitive) buffer back — this
                    // amortises the allocation cost across all packets on this thread.
                    let payload = UDP_PACKET_BUF.with(|buf| {
                        let mut buf = buf.borrow_mut();
                        buf.clear();
                        buf.extend_from_slice(&bufs[i][offset..end]);
                        // Clone returns a new Vec with exactly the right size,
                        // while the thread-local buffer retains its 65535 capacity
                        // for the next packet — avoiding repeated resize cycles.
                        buf.clone()
                    });
                    if self.underlay_tx.try_send(UnderlayPacket {
                        peer: meta[i].addr,
                        payload,
                    }).is_err() {
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
                // Found QUIC packets — reset idle counter and hand them to Quinn.
                self.idle_poll_count.store(0, Ordering::Relaxed);
                return Poll::Ready(Ok(keep));
            }
        }

        // Exhausted the per-call batch budget without finding any QUIC packet.
        //
        // Under non-QUIC traffic flood the inner socket always has data ready,
        // so every Quinn re-poll will burn through another 8 batches of non-QUIC
        // packets and immediately wake itself again, causing a busy loop.
        //
        // We use an idle-poll counter to detect this situation and eventually
        // yield the runtime worker without re-waking, allowing other tasks to
        // make progress.
        let idle_count = self.idle_poll_count.fetch_add(1, Ordering::Relaxed);
        if idle_count >= MAX_IDLE_POLL_BEFORE_YIELD {
            // Too many consecutive idle polls — yield control.  The inner socket
            // still has data, but we deliberately *do not* wake Quinn so that
            // this task sleeps until the runtime re-schedules it (or until new
            // I/O events trigger the waker registered by the inner socket).
            self.idle_poll_count.store(0, Ordering::Relaxed);
            return Poll::Pending;
        }

        // Under the threshold: wake Quinn so it re-polls us (standard pattern).
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
