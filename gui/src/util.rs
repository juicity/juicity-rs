/// Splits a "host:port" string into (host, port) parts.
///
/// Handles IPv6 addresses like `[::1]:1080` correctly.
pub fn split_host_port(addr: &str) -> (&str, u16) {
    if let Some(rest) = addr.strip_prefix('[') {
        // IPv6: [::1]:1080
        if let Some(pos) = rest.find("]:") {
            let host = &addr[1..][..pos]; // strip leading '[' only
            let port_str = &rest[pos + 2..];
            let port = port_str.parse().unwrap_or(0);
            (host, port)
        } else {
            // IPv6 without port: [::1]
            let host = &addr[1..addr.len().saturating_sub(1)];
            (host, 0)
        }
    } else {
        // IPv4 or hostname: 127.0.0.1:1080 or example.com:443
        if let Some(pos) = addr.rfind(':') {
            let host = &addr[..pos];
            let port_str = &addr[pos + 1..];
            let port = port_str.parse().unwrap_or(0);
            (host, port)
        } else {
            // No port
            (addr, 0)
        }
    }
}

/// Format host and port into a "host:port" string, adding brackets for IPv6 addresses.
///
/// For example:
/// - `("::1", 1080)` → `"[::1]:1080"`
/// - `("127.0.0.1", 1080)` → `"127.0.0.1:1080"`
/// - `("example.com", 443)` → `"example.com:443"`
pub fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port) // IPv6
    } else {
        format!("{}:{}", host, port) // IPv4 or hostname
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_host_port_ipv4() {
        assert_eq!(split_host_port("127.0.0.1:1080"), ("127.0.0.1", 1080));
    }

    #[test]
    fn test_split_host_port_ipv6() {
        assert_eq!(split_host_port("[::1]:1080"), ("::1", 1080));
    }

    #[test]
    fn test_split_host_port_ipv6_no_port() {
        assert_eq!(split_host_port("[::1]"), ("::1", 0));
    }

    #[test]
    fn test_split_host_port_hostname() {
        assert_eq!(split_host_port("example.com:443"), ("example.com", 443));
    }

    #[test]
    fn test_split_host_port_no_port() {
        assert_eq!(split_host_port("example.com"), ("example.com", 0));
    }
}
