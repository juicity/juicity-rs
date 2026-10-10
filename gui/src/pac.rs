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
use std::io::{BufRead, BufReader, Write};
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
    let listener = TcpListener::bind(listen_addr)
        .with_context(|| format!("PAC server: bind {listen_addr}"))?;

    let content: PacContent = Arc::new(Mutex::new(initial_content));
    let content_thread = Arc::clone(&content);

    let thread = std::thread::Builder::new()
        .name("pac-server".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };

                // Read (and discard) the request headers before replying. Closing
                // a socket that still has unread data in its receive buffer makes
                // Windows send an RST, which can reset or truncate the response we
                // are about to write. The short timeout stops a silent client from
                // stalling the single-threaded accept loop.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,                                   // peer closed
                            Ok(_) if line == "\r\n" || line == "\n" => break, // end of headers
                            Ok(_) => {}
                            Err(_) => break, // timeout or reset
                        }
                    }
                }

                let pac = content_thread.lock().map(|c| c.clone()).unwrap_or_default();
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
                let _ = stream.shutdown(std::net::Shutdown::Write);
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
    // Download to a temp file first, then perform atomic rename.
    // `with_extension` replaces the extension, so pass `"tmp"` (not `".tmp"`)
    // to get `china-list.tmp` rather than `china-list..tmp`.
    let tmp_path = dest.with_extension("tmp");
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
/// * `mixed_addr` – the local mixed inbound address (e.g. `127.0.0.1:1080`),
///   which serves both SOCKS5 and HTTP proxy
/// * `direct`     – domains that should connect directly (used in BypassChina)
/// * `proxy`      – domains that must be proxied (used in ProxyGfw)
pub fn generate_pac(
    mode: PacRuleMode,
    mixed_addr: &str,
    direct: &[String],
    proxy: &[String],
) -> String {
    match mode {
        PacRuleMode::BypassChina => generate_bypass_china_pac(mixed_addr, direct),
        PacRuleMode::ProxyGfw => generate_proxy_gfw_pac(mixed_addr, proxy),
    }
}

fn generate_bypass_china_pac(mixed_addr: &str, direct_domains: &[String]) -> String {
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
    return "SOCKS5 {mixed_addr}; PROXY {mixed_addr}; DIRECT";
}}"#
    )
}

fn generate_proxy_gfw_pac(mixed_addr: &str, proxy_domains: &[String]) -> String {
    let domains_js = domains_to_js_object(proxy_domains);
    format!(
        r#"/* PAC – GFW List Only (generated by juicity-gui) */
var proxyDomains = {domains_js};
function FindProxyForURL(url, host) {{
    host = host.toLowerCase();
    var parts = host.split('.');
    for (var i = 0; i < parts.length - 1; i++) {{
        var d = parts.slice(i).join('.');
        if (proxyDomains[d]) return "SOCKS5 {mixed_addr}; PROXY {mixed_addr}; DIRECT";
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// Bind an ephemeral loopback port, then release it so the PAC server can
    /// take it over; returns the `host:port` address.
    fn free_addr() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("127.0.0.1:{port}")
    }

    /// Minimal HTTP GET; the PAC server closes the connection, so read to EOF.
    fn http_get(addr: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// Fetch the same URL with a real HTTP client, the way the OS proxy or a
    /// browser would, to catch protocol-level problems the raw client misses.
    fn http_client_get(addr: &str, path: &str) -> String {
        ureq::get(&format!("http://{addr}{path}"))
            .call()
            .expect("HTTP client request failed")
            .into_string()
            .expect("HTTP client could not read the body")
    }

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

    /// The local inbound is a mixed port, so the PAC offers the HTTP proxy on
    /// the same address as a fallback for clients that ignore SOCKS5.
    #[test]
    fn generated_pac_offers_both_socks5_and_http_proxy() {
        for mode in [PacRuleMode::BypassChina, PacRuleMode::ProxyGfw] {
            let pac = generate_pac(mode, "127.0.0.1:1080", &[], &[]);
            assert!(
                pac.contains("SOCKS5 127.0.0.1:1080; PROXY 127.0.0.1:1080; DIRECT"),
                "unexpected PAC proxy chain for {mode:?}"
            );
        }
    }

    /// End-to-end: rule files on disk are parsed, embedded in the PAC and that
    /// exact PAC is what the HTTP server hands to a client.
    #[test]
    fn rules_file_is_converted_and_served_as_pac() {
        let dir = std::env::temp_dir().join(format!("juicity-pac-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Written exactly like `download_file` leaves them: raw rule text.
        std::fs::write(
            dir.join("china-list.txt"),
            "# comment\nbaidu.com\nfull:qq.com\nregexp:^skip\n",
        )
        .unwrap();
        std::fs::write(dir.join("gfw.txt"), "twitter.com\ndomain:google.com\n").unwrap();

        let (direct, proxy) = load_rules(&dir);
        assert_eq!(direct, vec!["baidu.com", "qq.com"]);
        assert_eq!(proxy, vec!["twitter.com", "google.com"]);

        let pac = generate_pac(PacRuleMode::BypassChina, "127.0.0.1:1080", &direct, &proxy);
        assert!(pac.contains("\"baidu.com\":1"), "domain list was not embedded in the PAC");

        let addr = free_addr();
        let server = start(&addr, pac.clone()).expect("PAC server failed to bind");

        let response = http_get(&addr, "/pac");
        assert!(response.starts_with("HTTP/1.0 200 OK"), "{response}");
        assert!(response.contains("Content-Type: application/x-ns-proxy-autoconfig"));
        assert!(response.contains(&format!("Content-Length: {}", pac.len())));
        assert!(response.ends_with(&pac), "served body differs from the generated PAC");
        assert_eq!(http_client_get(&addr, "/pac"), pac, "HTTP client got a different body");

        // `update` must change what is served.
        server.update("/* updated */".to_string());
        assert!(http_get(&addr, "/pac").ends_with("/* updated */"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manual check against the rule files this machine's app actually
    /// downloaded. Ignored by default because it depends on local app data;
    /// run it with:
    ///
    /// ```text
    /// cargo test -p juicity-gui real_downloaded_rules -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs the app's downloaded rule files; run manually"]
    fn real_downloaded_rules_convert_and_serve() {
        let paths = crate::config::ConfigPaths::discover().expect("resolve config dir");
        let (direct, proxy) = load_rules(&paths.config_dir);
        println!("china-list -> {} domains, gfw -> {} domains", direct.len(), proxy.len());
        assert!(direct.len() > 1000, "china-list did not convert ({} domains)", direct.len());
        assert!(proxy.len() > 1000, "gfw list did not convert ({} domains)", proxy.len());

        let pac = generate_pac(PacRuleMode::BypassChina, "127.0.0.1:1080", &direct, &proxy);
        println!("generated PAC is {} bytes", pac.len());
        assert!(pac.len() > 100_000, "generated PAC looks too small: {} bytes", pac.len());
        assert!(
            direct.iter().any(|d| pac.contains(d.as_str())),
            "no rule from the list appears in the PAC"
        );

        let addr = free_addr();
        let server = start(&addr, pac.clone()).expect("PAC server failed to bind");
        let response = http_get(&addr, "/pac");
        assert!(response.ends_with(&pac), "served PAC differs from the generated one");
        assert_eq!(http_client_get(&addr, "/pac"), pac, "HTTP client got a different body");
        println!("served {} bytes over http://{addr}/pac", pac.len());
        drop(server);

        // Optionally write the PAC out so it can be inspected or syntax-checked
        // with a real JavaScript engine, e.g. `node --check` / `d8`:
        //   $env:JUICITY_PAC_DUMP = "$env:TEMP\juicity.pac"
        if let Ok(path) = std::env::var("JUICITY_PAC_DUMP") {
            std::fs::write(&path, &pac).expect("failed to write PAC dump");
            println!("dumped PAC to {path}");
        }
    }
}
