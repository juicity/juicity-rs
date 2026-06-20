use crate::config::Config;
use url::form_urlencoded::Serializer;

/// Generate a Juicity share link from configuration.
///
/// Format: `juicity://uuid:password@host:port?params`
///
/// For server configs, the first user entry is used.
///
/// # Parameters
/// - `config`: The configuration to generate a link from.
/// - `host_override`: If provided, replaces the host portion of the link.
///   Useful when the server's listen address is not the desired public host.
/// - `sni_override`: If provided, replaces the SNI query parameter.
///   Useful when the SNI should differ from the host (e.g., CDN setups).
pub fn generate_share_link(
    config: &Config,
    host_override: Option<&str>,
    sni_override: Option<&str>,
    cert_sha256_override: Option<&str>,
) -> Result<String, String> {
    // Determine uuid and password
    let (uuid, password) = if !config.uuid.is_empty() && !config.password.is_empty() {
        (config.uuid.clone(), config.password.clone())
    } else if let Some((uid, pw)) = config.users.iter().next() {
        (uid.clone(), pw.clone())
    } else {
        return Err("No valid user credentials found in config".to_string());
    };

    // Parse server host:port; for server configs, fall back to the listen address
    let server_addr = if !config.server.is_empty() {
        &config.server
    } else {
        &config.listen
    };
    let (base_host, port) = parse_host_port(server_addr)?;

    // Apply host_override if provided
    let host = host_override.unwrap_or(&base_host);

    // URL-encode uuid and password
    let encoded_uuid = Serializer::new(String::new())
        .append_pair("", &uuid)
        .finish();
    // Remove the leading '=' from the encoded pair
    let encoded_uuid = encoded_uuid
        .strip_prefix('=')
        .unwrap_or(&encoded_uuid)
        .to_string();

    let encoded_password = Serializer::new(String::new())
        .append_pair("", &password)
        .finish();
    let encoded_password = encoded_password
        .strip_prefix('=')
        .unwrap_or(&encoded_password)
        .to_string();

    // Build query parameters
    let mut query_parts: Vec<String> = Vec::new();

    // sni (required) — sni_override takes highest priority
    let sni = sni_override
        .or_else(|| {
            if !config.sni.is_empty() {
                Some(config.sni.as_str())
            } else {
                None
            }
        })
        .unwrap_or(&host);
    query_parts.push(format!("sni={}", url_encode_param(sni)));

    // congestion_control
    if !config.congestion_control.is_empty() {
        query_parts.push(format!(
            "congestion_control={}",
            url_encode_param(&config.congestion_control)
        ));
    }

    // allow_insecure
    query_parts.push(format!(
        "allow_insecure={}",
        if config.allow_insecure { "1" } else { "0" }
    ));

    // pinned_certchain_sha256 (optional)
    // cert_sha256_override takes priority over config value
    let cert_sha256 = cert_sha256_override
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if !config.pinned_certchain_sha256.is_empty() {
                Some(config.pinned_certchain_sha256.as_str())
            } else {
                None
            }
        });
    if let Some(sha256) = cert_sha256 {
        query_parts.push(format!(
            "pinned_certchain_sha256={}",
            url_encode_param(sha256)
        ));
    }

    let query_string = query_parts.join("&");

    let link = format!(
        "juicity://{}:{}@{}:{}?{}",
        encoded_uuid, encoded_password, host, port, query_string
    );

    Ok(link)
}

/// Print a QR code to the terminal using Unicode half-block characters.
///
/// Each output character represents a 1-wide × 2-tall block of QR modules,
/// using the mapping:
///   (light, light) → ' '   (dark, light) → '▀'
///   (light, dark)  → '▄'   (dark, dark)  → '█'
///
/// This halves the row count so the rendered image is roughly square in
/// a standard monospace terminal (where character cells are ~2× taller
/// than they are wide).
pub fn print_qrcode(link: &str) -> Result<(), anyhow::Error> {
    use qrcode::Color;

    let code = qrcode::QrCode::new(link.as_bytes())?;
    let width = code.width();

    // Quiet zone: 2 modules on each side (spec recommends 4; 2 is sufficient
    // for most scanners and keeps the output compact).
    let border = 2usize;
    let total = width + 2 * border;

    // Returns true if the module at (row, col) in the bordered grid is dark.
    let is_dark = |row: usize, col: usize| -> bool {
        if row < border || row >= border + width || col < border || col >= border + width {
            false // quiet zone is always light
        } else {
            code[(row - border, col - border)] == Color::Dark
        }
    };

    let mut output = String::new();
    let mut row = 0usize;
    while row < total {
        for col in 0..total {
            let top = is_dark(row, col);
            let bot = if row + 1 < total { is_dark(row + 1, col) } else { false };
            output.push(match (top, bot) {
                (false, false) => ' ',
                (true,  false) => '\u{2580}', // ▀
                (false, true)  => '\u{2584}', // ▄
                (true,  true)  => '\u{2588}', // █
            });
        }
        output.push('\n');
        row += 2;
    }

    print!("{}", output);
    Ok(())
}

/// Save a QR code as a PNG image to the specified path.
pub fn save_qrcode_png(link: &str, path: &str) -> Result<(), anyhow::Error> {
    let code = qrcode::QrCode::new(link.as_bytes())?;
    let image = code.render::<image::Luma<u8>>().build();
    image.save(path)?;
    println!("QR code saved to: {}", path);
    Ok(())
}

// ── Helpers ──

/// Parse a `host:port` string into `(host, port)`.
pub fn parse_host_port(addr: &str) -> Result<(String, u16), String> {
    // Handle IPv6 addresses like [::1]:443
    if addr.starts_with('[') {
        if let Some(close_bracket) = addr.find(']') {
            if close_bracket + 1 < addr.len() && addr.as_bytes()[close_bracket + 1] == b':' {
                let host = addr[1..close_bracket].to_string();
                let port_str = &addr[close_bracket + 2..];
                let port: u16 = port_str
                    .parse()
                    .map_err(|_| format!("Invalid port in address: {}", addr))?;
                return Ok((host, port));
            }
        }
        return Err(format!("Invalid IPv6 address format: {}", addr));
    }

    // Standard host:port
    if let Some(colon_pos) = addr.rfind(':') {
        let host = addr[..colon_pos].to_string();
        let port_str = &addr[colon_pos + 1..];
        if port_str.is_empty() || port_str.chars().any(|c| !c.is_ascii_digit()) {
            return Err(format!("Invalid port in address: {}", addr));
        }
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid port in address: {}", addr))?;
        Ok((host, port))
    } else {
        Err(format!(
            "Address must be in host:port format, got: {}",
            addr
        ))
    }
}

/// URL-encode a single query parameter value.
fn url_encode_param(value: &str) -> String {
    Serializer::new(String::new())
        .append_pair("", value)
        .finish()
        .strip_prefix('=')
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_generate_share_link_client() {
        let mut config = Config::default();
        config.server = "example.com:443".to_string();
        config.uuid = "00000000-0000-0000-0000-000000000000".to_string();
        config.password = "test-password".to_string();
        config.sni = "example.com".to_string();
        config.congestion_control = "bbr".to_string();
        config.allow_insecure = false;

        let link = generate_share_link(&config, None, None, None).unwrap();
        assert!(link.starts_with("juicity://"));
        assert!(link.contains("00000000-0000-0000-0000-000000000000"));
        assert!(link.contains("test-password"));
        assert!(link.contains("example.com:443"));
        assert!(link.contains("sni=example.com"));
        assert!(link.contains("congestion_control=bbr"));
        assert!(link.contains("allow_insecure=0"));
    }

    #[test]
    fn test_generate_share_link_server() {
        let mut config = Config::default();
        config.server = "server.example.com:8443".to_string();
        config.sni = "server.example.com".to_string();
        config.congestion_control = "cubic".to_string();
        config.allow_insecure = true;
        config.pinned_certchain_sha256 =
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string();

        let mut users = HashMap::new();
        users.insert(
            "11111111-1111-1111-1111-111111111111".to_string(),
            "server-pw".to_string(),
        );
        config.users = users;

        let link = generate_share_link(&config, None, None, None).unwrap();
        assert!(link.starts_with("juicity://"));
        assert!(link.contains("11111111-1111-1111-1111-111111111111"));
        assert!(link.contains("server-pw"));
        assert!(link.contains("server.example.com:8443"));
        assert!(link.contains("allow_insecure=1"));
        assert!(link.contains("pinned_certchain_sha256=abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"));
    }

    #[test]
    fn test_parse_host_port() {
        let (host, port) = parse_host_port("example.com:443").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443u16);
    }

    #[test]
    fn test_parse_host_port_ipv6() {
        let (host, port) = parse_host_port("[::1]:443").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 443u16);
    }

    #[test]
    fn test_parse_host_port_invalid() {
        assert!(parse_host_port("no-port").is_err());
        assert!(parse_host_port("").is_err());
    }

    #[test]
    fn test_url_encode_special_chars() {
        let encoded = url_encode_param("a b+c");
        assert_eq!(encoded, "a+b%2Bc");
    }

    #[test]
    fn test_no_credentials_error() {
        let config = Config::default();
        let result = generate_share_link(&config, None, None, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("No valid user credentials found"));
    }
}
