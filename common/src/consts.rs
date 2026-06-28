use std::time::Duration;

/// Default dial timeout
pub const DEFAULT_DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Default NAT timeout for UDP (3 minutes, compatible with Go version)
pub const DEFAULT_NAT_TIMEOUT: Duration = Duration::from_secs(180);
/// DNS query timeout (17 seconds, RFC 5452)
pub const DNS_QUERY_TIMEOUT: Duration = Duration::from_secs(17);
/// Ethernet MTU
pub const ETHERNET_MTU: usize = 1500;
/// Authentication timeout
pub const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(10);
/// In-flight underlay TTL
pub const IN_FLIGHT_UNDERLAY_TTL: Duration = Duration::from_secs(10);
/// In-flight underlay evict timeout (100ms)
/// Controls how long evict() waits for an underlay auth to arrive
/// before giving up on the corresponding UDP packet.
pub const IN_FLIGHT_UNDERLAY_EVICT_TIMEOUT: Duration = Duration::from_millis(100);
/// Default congestion control window
pub const DEFAULT_CWND: u64 = 10;
/// Max open incoming streams
pub const MAX_OPEN_INCOMING_STREAMS: u64 = 100;
/// Keep-alive period for QUIC
pub const KEEP_ALIVE_PERIOD: Duration = Duration::from_secs(10);

/// QUIC per-stream receive window (bytes).
/// Lowering this bounds worst-case memory per stream in high-throughput scenarios.
pub const QUIC_STREAM_RECEIVE_WINDOW: u32 = 512 * 1024;
/// QUIC per-connection receive window (bytes).
/// Must be >= stream window; limits aggregate receive buffering per connection.
pub const QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 8 * 1024 * 1024;
/// QUIC send window (bytes).
/// Caps unacknowledged outbound data retained in memory per connection.
pub const QUIC_SEND_WINDOW: u64 = 8 * 1024 * 1024;

/// JUICIY protocol version 0
pub const JUICIY_VERSION_0: u8 = 0;
/// Underlay salt length
pub const UNDERLAY_SALT_LEN: usize = 32;
/// Maximum number of cached (domain, port) -> SocketAddr entries per UDP relay stream.
/// Prevents domain_ip_map from growing without bound across long-lived UDP sessions.
pub const MAX_UDP_DNS_CACHE: usize = 256;
/// TTL for DNS cache entries (5 minutes).
/// After this duration, a cached DNS result is considered stale and will be re-resolved.
pub const UDP_DNS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Maximum number of concurrent inbound TCP connections on the local proxy listener.
/// Matches the UDP concurrency limit in the Forwarder to provide consistent back-pressure.
pub const MAX_CONCURRENT_TCP_CONNECTIONS: usize = 256;

/// Maximum number of in-flight underlay auth entries.
/// Guards against a burst of forged/unanswered underlay auth packets filling memory
/// during the 5-second cleanup window.
pub const MAX_IN_FLIGHT_UNDERLAY_ENTRIES: usize = 10_000;

/// Soft capacity for the UDP endpoint pool.
/// When reached, the least-recently-used endpoint is evicted.
pub const MAX_UDP_ENDPOINTS: usize = 5_000;

/// Soft capacity for non-QUIC underlay sessions.
/// When reached, the least-recently-used session is evicted.
pub const MAX_UNDERLAY_SESSIONS: usize = 5_000;

/// Maximum concurrent non-QUIC underlay handler tasks.
/// Bounds memory used by in-flight packet handler futures during bursts.
pub const MAX_UNDERLAY_HANDLER_CONCURRENCY: usize = 1_024;

/// QUIC idle timeout — defense-in-depth against connections that authenticate but
/// never open streams.  Set higher than the stream-accept timeout so the QUIC
/// transport layer acts as a second line of defence.
pub const MAX_QUIC_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
