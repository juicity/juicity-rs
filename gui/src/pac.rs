//! PAC (Proxy Auto-Config) file generation and serving.
//!
//! Rule lists are downloaded from Loyalsoldier/v2ray-rules-dat:
//!   - direct-list.txt  → domains that should bypass the proxy (Bypass-China mode)
//!   - proxy-list.txt   → domains that must go through the proxy (GFW-List mode)
//!
//! The generated PAC file is served by a tiny background HTTP server on
//! `AppConfig::pac_listen` (default `127.0.0.1:1090`).

use crate::config::PacRuleMode;
use anyhow::Context;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Shared PAC content that the HTTP server thread reads on every request.
pub type PacContent = Arc<Mutex<String>>;

/// Handle for the background PAC HTTP server.  Dropping this struct does **not**
/// stop the server thread (the thread holds its own Arc clone), but the OS will
/// reclaim everything on process exit.
pub struct PacServer {
    /// Live PAC content – write here to update what the server serves.
    pub content: PacContent,
    // Keep the JoinHandle so the thread is at least not orphaned silently.
    _thread: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for PacServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacServer").finish_non_exhaustive()
    }
}

impl PacServer {
    /// Replace the PAC content served to browsers.
    pub fn update(&self, new_content: String) {
        if let Ok(mut c) = self.content.lock() {
            *c = new_content;
        }
    }
}

// ── Server ────────────────────────────────────────────────────────────────────

/// Bind a TCP listener on `listen_addr` and start a background thread that
/// serves the current PAC content as an HTTP response.
pub fn start(listen_addr: &str, initial_content: String) -> anyhow::Result<PacServer> {
    let listener =
        TcpListener::bind(listen_addr).with_context(|| format!("PAC server: bind {listen_addr}"))?;

    let content: PacContent = Arc::new(Mutex::new(initial_content));
    let content_thread = Arc::clone(&content);

    let thread = std::thread::Builder::new()
        .name("pac-server".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let pac = content_thread
                    .lock()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                // Minimal HTTP/1.0 response – no keep-alive needed.
                let response = format!(
                    "HTTP/1.0 200 OK\r\n\
                     Content-Type: application/x-ns-proxy-autoconfig\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    pac.len(),
                    pac
                );
                let _ = stream.write_all(response.as_bytes());
            }
        })
        .context("failed to spawn pac-server thread")?;

    Ok(PacServer {
        content,
        _thread: thread,
    })
}

/// URL that browsers/system proxy should be configured with.
pub fn pac_url(listen_addr: &str) -> String {
    format!("http://{}/pac", listen_addr)
}

// ── Rule download ─────────────────────────────────────────────────────────────

pub const DIRECT_LIST_URL: &str =
    "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/direct-list.txt";
pub const PROXY_LIST_URL: &str =
    "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/proxy-list.txt";

/// Return how many hours ago the downloaded rule files were last modified.
/// Returns `None` if the files don't exist yet.
pub fn rules_age_hours(data_dir: &Path) -> Option<u64> {
    let meta = std::fs::metadata(data_dir.join("china-list.txt")).ok()?;
    let elapsed = meta.modified().ok()?.elapsed().ok()?;
    Some(elapsed.as_secs() / 3600)
}

/// Download fresh rule lists into `data_dir` (blocking, intended for a
/// background thread).  Returns `(direct_count, proxy_count)` on success.
///
/// `direct_url` and `proxy_url` override the built-in defaults and allow
/// users to specify custom mirror URLs.
pub fn download_rules(
    data_dir: &Path,
    direct_url: &str,
    proxy_url: &str,
) -> anyhow::Result<(usize, usize)> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create data_dir {}", data_dir.display()))?;

    let china_path = data_dir.join("china-list.txt");
    let gfw_path = data_dir.join("gfw.txt");

    download_file(direct_url, &china_path)
        .context("failed to download direct-list (china-list)")?;
    download_file(proxy_url, &gfw_path).context("failed to download proxy-list (gfw)")?;

    let direct = parse_domain_list(&std::fs::read_to_string(&china_path)?);
    let proxy = parse_domain_list(&std::fs::read_to_string(&gfw_path)?);
    Ok((direct.len(), proxy.len()))
}

/// Load rule lists that were previously downloaded to `data_dir`.
/// Returns empty Vecs if the files are missing.
pub fn load_rules(data_dir: &Path) -> (Vec<String>, Vec<String>) {
    let read = |file: &str| -> Vec<String> {
        std::fs::read_to_string(data_dir.join(file))
            .map(|s| parse_domain_list(&s))
            .unwrap_or_default()
    };
    (read("china-list.txt"), read("gfw.txt"))
}

// ── Download helper ───────────────────────────────────────────────────────────

fn download_file(url: &str, dest: &Path) -> anyhow::Result<()> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("Failed to download {}: {}", url, e))?;

    let mut reader = response.into_reader();
    // Download to a temp file first, then perform atomic rename
    let tmp_path = dest.with_extension(".tmp");
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        std::io::copy(&mut reader, &mut file)?;
    }
    std::fs::rename(&tmp_path, dest)?;
    Ok(())
}

// ── Domain list parsing ───────────────────────────────────────────────────────

fn parse_domain_list(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            // v2ray-rules-dat prefixes:
            //   full:example.com  → exact hostname match
            //   domain:example.com → subdomain match (same as bare)
            //   regexp:...        → skip (not supported in PAC)
            //   keyword:...       → skip
            if l.starts_with("regexp:") || l.starts_with("keyword:") {
                None
            } else if let Some(d) = l.strip_prefix("full:") {
                Some(d.to_string())
            } else if let Some(d) = l.strip_prefix("domain:") {
                Some(d.to_string())
            } else if l.contains(':') {
                None // unknown prefix
            } else {
                Some(l.to_string())
            }
        })
        .collect()
}

// ── PAC generation ────────────────────────────────────────────────────────────

/// Generate a PAC file string.
///
/// * `mode`       – which rule set to apply
/// * `socks_addr` – the local SOCKS5 address to proxy through (e.g. `127.0.0.1:1080`)
/// * `direct`     – domains that should connect directly (used in BypassChina)
/// * `proxy`      – domains that must be proxied (used in ProxyGfw)
pub fn generate_pac(
    mode: PacRuleMode,
    socks_addr: &str,
    direct: &[String],
    proxy: &[String],
) -> String {
    match mode {
        PacRuleMode::BypassChina => generate_bypass_china_pac(socks_addr, direct),
        PacRuleMode::ProxyGfw => generate_proxy_gfw_pac(socks_addr, proxy),
    }
}

fn generate_bypass_china_pac(socks_addr: &str, direct_domains: &[String]) -> String {
    let domains_js = domains_to_js_object(direct_domains);
    format!(
        r#"/* PAC – Bypass China (generated by juicity-gui) */
var directDomains = {domains_js};
function FindProxyForURL(url, host) {{
    host = host.toLowerCase();
    var parts = host.split('.');
    for (var i = 0; i < parts.length - 1; i++) {{
        var d = parts.slice(i).join('.');
        if (directDomains[d]) return "DIRECT";
    }}
    return "SOCKS5 {socks_addr}; SOCKS {socks_addr}; DIRECT";
}}"#
    )
}

fn generate_proxy_gfw_pac(socks_addr: &str, proxy_domains: &[String]) -> String {
    let domains_js = domains_to_js_object(proxy_domains);
    format!(
        r#"/* PAC – GFW List Only (generated by juicity-gui) */
var proxyDomains = {domains_js};
function FindProxyForURL(url, host) {{
    host = host.toLowerCase();
    var parts = host.split('.');
    for (var i = 0; i < parts.length - 1; i++) {{
        var d = parts.slice(i).join('.');
        if (proxyDomains[d]) return "SOCKS5 {socks_addr}; SOCKS {socks_addr}; DIRECT";
    }}
    return "DIRECT";
}}"#
    )
}

fn domains_to_js_object(domains: &[String]) -> String {
    let entries: String = domains
        .iter()
        .map(|d| {
            // Sanitise: remove any embedded quotes to avoid JS injection.
            let safe = d.replace('"', "");
            format!("\"{}\":1", safe)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{}}}", entries)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_prefixes() {
        let raw = "# comment\nbaidu.com\nfull:qq.com\ndomain:taobao.com\nregexp:^abc\nkeyword:vpn";
        let domains = parse_domain_list(raw);
        assert_eq!(domains, vec!["baidu.com", "qq.com", "taobao.com"]);
    }

    #[test]
    fn generate_bypass_china_contains_direct() {
        let pac = generate_pac(
            PacRuleMode::BypassChina,
            "127.0.0.1:1080",
            &["baidu.com".to_string()],
            &[],
        );
        assert!(pac.contains("\"baidu.com\":1"));
        assert!(pac.contains("return \"DIRECT\""));
        assert!(pac.contains("SOCKS5 127.0.0.1:1080"));
    }

    #[test]
    fn generate_proxy_gfw_contains_proxy() {
        let pac = generate_pac(
            PacRuleMode::ProxyGfw,
            "127.0.0.1:1080",
            &[],
            &["twitter.com".to_string()],
        );
        assert!(pac.contains("\"twitter.com\":1"));
        assert!(pac.contains("return \"DIRECT\""));
        assert!(pac.contains("SOCKS5 127.0.0.1:1080"));
    }
}
