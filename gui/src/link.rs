use crate::config::{ProxyProfile, ProxyProtocol};
use anyhow::{bail, Context};
use base64::Engine;

/// All parsed fields from an imported share link.
#[derive(Debug, Clone)]
pub struct ImportedShareLink {
    pub protocol: ProxyProtocol,
    /// Remarks / display name (from URL fragment).
    pub name: String,
    pub server: String,
    pub server_port: u16,
    pub password: String,
    // Juicity-specific
    pub uuid: String,
    pub sni: Option<String>,
    pub allow_insecure: bool,
    pub pinned_certchain_sha256: Option<String>,
    pub congestion_control: Option<String>,
    // Shadowsocks-specific
    pub method: String,
    pub plugin: Option<String>,
    pub plugin_opts: Option<String>,
}

impl ImportedShareLink {
    /// Merge the parsed link into an existing profile, preserving fields not set by the link.
    pub fn apply_to(&self, profile: &mut ProxyProfile) {
        profile.protocol = self.protocol;
        if !self.name.is_empty() && self.name != "New Server" {
            profile.name = self.name.clone();
        }
        profile.server = self.server.clone();
        profile.server_port = self.server_port;
        profile.password = self.password.clone();
        profile.uuid = self.uuid.clone();
        profile.sni = self.sni.clone();
        profile.allow_insecure = self.allow_insecure;
        profile.pinned_certchain_sha256 = self.pinned_certchain_sha256.clone();
        profile.congestion_control = self.congestion_control.clone();
        if !self.method.is_empty() {
            profile.method = self.method.clone();
        }
        profile.plugin = self.plugin.clone();
        profile.plugin_opts = self.plugin_opts.clone();
    }
}

pub fn import_share_link(input: &str) -> anyhow::Result<ImportedShareLink> {
    let raw = input.trim();
    if raw.starts_with("juicity://") {
        return parse_juicity_link(raw);
    }
    if raw.starts_with("ss://") {
        return parse_ss_link(raw);
    }
    bail!("unsupported link scheme: only juicity:// and ss:// are accepted")
}

pub fn export_share_link(profile: &ProxyProfile) -> anyhow::Result<String> {
    match profile.protocol {
        ProxyProtocol::Juicity => export_juicity_link(profile),
        ProxyProtocol::Shadowsocks => export_ss_link(profile),
    }
}

fn export_juicity_link(profile: &ProxyProfile) -> anyhow::Result<String> {
    if profile.server.is_empty() || profile.uuid.is_empty() || profile.password.is_empty() {
        bail!("juicity profile is missing server/uuid/password");
    }
    let userinfo = format!(
        "{}:{}",
        url::form_urlencoded::byte_serialize(profile.uuid.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(profile.password.as_bytes()).collect::<String>(),
    );
    let mut s = format!(
        "juicity://{}@{}",
        userinfo,
        crate::util::format_host_port(&profile.server, profile.server_port),
    );
    let mut params = Vec::new();
    if let Some(sni) = &profile.sni {
        if !sni.is_empty() {
            params.push(format!(
                "sni={}",
                url::form_urlencoded::byte_serialize(sni.as_bytes()).collect::<String>()
            ));
        }
    }
    if profile.allow_insecure {
        params.push("allowInsecure=1".to_string());
    }
    // Parameter names follow `juicity_common::link`.
    for (key, value) in [
        ("congestion_control", &profile.congestion_control),
        ("pinned_certchain_sha256", &profile.pinned_certchain_sha256),
    ] {
        if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
            params.push(format!(
                "{key}={}",
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            ));
        }
    }
    if !params.is_empty() {
        s.push('?');
        s.push_str(&params.join("&"));
    }
    if !profile.name.is_empty() && profile.name != "New Server" {
        s.push('#');
        s.push_str(
            &url::form_urlencoded::byte_serialize(profile.name.as_bytes()).collect::<String>(),
        );
    }
    Ok(s)
}

fn export_ss_link(profile: &ProxyProfile) -> anyhow::Result<String> {
    if profile.server.is_empty() || profile.password.is_empty() {
        bail!("shadowsocks profile is missing server/password");
    }
    // SIP002 format: ss://method:password@host:port[#remarks]
    let userinfo = format!("{}:{}", profile.method, profile.password);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(userinfo.as_bytes());
    let mut s = format!(
        "ss://{}@{}",
        b64,
        crate::util::format_host_port(&profile.server, profile.server_port)
    );
    if let Some(plugin) = &profile.plugin {
        if !plugin.is_empty() {
            let mut plugin_str = plugin.clone();
            if let Some(opts) = &profile.plugin_opts {
                if !opts.is_empty() {
                    plugin_str.push(';');
                    plugin_str.push_str(opts);
                }
            }
            s.push_str(&format!(
                "?plugin={}",
                url::form_urlencoded::byte_serialize(plugin_str.as_bytes()).collect::<String>()
            ));
        }
    }
    if !profile.name.is_empty() && profile.name != "New Server" {
        s.push('#');
        s.push_str(
            &url::form_urlencoded::byte_serialize(profile.name.as_bytes()).collect::<String>(),
        );
    }
    Ok(s)
}

fn parse_juicity_link(raw: &str) -> anyhow::Result<ImportedShareLink> {
    let url = url::Url::parse(raw).context("invalid juicity link")?;
    if url.scheme() != "juicity" {
        bail!("invalid juicity scheme")
    }

    let uuid = url.username().to_string();
    if uuid.is_empty() {
        bail!("juicity link missing uuid in userinfo")
    }
    let password = url.password().unwrap_or_default().to_string();
    if password.is_empty() {
        bail!("juicity link missing password in userinfo")
    }

    let host = url
        .host_str()
        .context("juicity link missing host")?
        .to_string();
    let port = url.port().context("juicity link missing port")?;

    let mut sni = None;
    let mut allow_insecure = false;
    let mut pinned_certchain_sha256 = None;
    let mut congestion_control = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "sni" => sni = Some(v.into_owned()),
            "pinned_certchain_sha256" if !v.is_empty() => {
                pinned_certchain_sha256 = Some(v.into_owned())
            }
            "congestion_control" => {
                congestion_control = crate::config::normalize_congestion_control(&v)
            }
            "allowInsecure" | "allow_insecure" => {
                allow_insecure = v == "1" || v.to_lowercase() == "true";
            }
            _ => {}
        }
    }

    let name = url
        .fragment()
        .filter(|v| !v.trim().is_empty())
        .map(percent_decode)
        .unwrap_or_else(|| format!("{}:{}", host, port));

    Ok(ImportedShareLink {
        protocol: ProxyProtocol::Juicity,
        name,
        server: host,
        server_port: port,
        password: percent_decode(&password),
        uuid: percent_decode(&uuid),
        sni,
        allow_insecure,
        pinned_certchain_sha256,
        congestion_control,
        method: String::new(),
        plugin: None,
        plugin_opts: None,
    })
}

fn parse_ss_link(raw: &str) -> anyhow::Result<ImportedShareLink> {
    let payload = &raw["ss://".len()..];

    if payload.contains('@') {
        parse_ss_sip002(raw)
    } else {
        parse_ss_legacy(raw)
    }
}

fn parse_ss_sip002(raw: &str) -> anyhow::Result<ImportedShareLink> {
    let url = url::Url::parse(raw).context("invalid ss link")?;
    if url.scheme() != "ss" {
        bail!("invalid ss scheme")
    }

    let host = url.host_str().context("ss link missing host")?.to_string();
    let port = url.port().context("ss link missing port")?;

    let username = url.username();
    if username.is_empty() {
        bail!("ss link missing user info")
    }

    let (method, password) = if let Some(plain_pass) = url.password() {
        // method is in username, password is in password field
        (percent_decode(username), percent_decode(plain_pass))
    } else {
        // base64(method:password) in username
        let decoded = decode_base64_variants(username)
            .context("ss link username must be plain method:password or base64(method:password)")?;
        let (m, p) = decoded
            .split_once(':')
            .context("ss credentials missing method:password pair")?;
        (m.to_string(), p.to_string())
    };

    // Parse plugin from query string
    let mut plugin = None;
    let mut plugin_opts = None;
    for (k, v) in url.query_pairs() {
        if k == "plugin" {
            let vstr = v.into_owned();
            if let Some((prog, opts)) = vstr.split_once(';') {
                plugin = Some(prog.to_string());
                plugin_opts = Some(opts.to_string());
            } else {
                plugin = Some(vstr);
            }
        }
    }

    let name = url
        .fragment()
        .filter(|v| !v.trim().is_empty())
        .map(percent_decode)
        .unwrap_or_else(|| format!("{}:{}", host, port));

    Ok(ImportedShareLink {
        protocol: ProxyProtocol::Shadowsocks,
        name,
        server: host,
        server_port: port,
        password,
        uuid: String::new(),
        sni: None,
        allow_insecure: false,
        pinned_certchain_sha256: None,
        congestion_control: None,
        method,
        plugin,
        plugin_opts,
    })
}

fn parse_ss_legacy(raw: &str) -> anyhow::Result<ImportedShareLink> {
    let content = &raw["ss://".len()..];
    let mut parts = content.splitn(2, '#');
    let encoded = parts.next().unwrap_or_default();
    let remark = parts
        .next()
        .filter(|v| !v.trim().is_empty())
        .map(percent_decode);

    let decoded = decode_base64_variants(encoded)
        .context("legacy ss link must be base64(method:password@host:port)")?;

    let at_pos = decoded
        .rfind('@')
        .context("legacy ss link decoded payload missing '@'")?;
    let creds = &decoded[..at_pos];
    let endpoint = &decoded[at_pos + 1..];

    let (method, password) = creds
        .split_once(':')
        .context("legacy ss credentials missing method:password pair")?;
    let (host, port) = crate::util::split_host_port(endpoint);

    let name = remark.unwrap_or_else(|| format!("{}:{}", host, port));

    Ok(ImportedShareLink {
        protocol: ProxyProtocol::Shadowsocks,
        name,
        server: host.to_string(),
        server_port: port,
        password: password.to_string(),
        uuid: String::new(),
        sni: None,
        allow_insecure: false,
        pinned_certchain_sha256: None,
        congestion_control: None,
        method: method.to_string(),
        plugin: None,
        plugin_opts: None,
    })
}

fn percent_decode(input: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(input.len());
    let mut iter = input.bytes().peekable();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let h1 = iter.next().unwrap_or(b'0');
            let h2 = iter.next().unwrap_or(b'0');
            let hex = format!("{}{}", h1 as char, h2 as char);
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
                continue;
            }
        }
        bytes.push(b);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| input.to_string())
}

fn decode_base64_variants(input: &str) -> anyhow::Result<String> {
    let normalized = input.trim();
    let engines = [
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::STANDARD,
    ];

    for engine in engines {
        if let Ok(bytes) = engine.decode(normalized) {
            if let Ok(text) = String::from_utf8(bytes) {
                return Ok(text);
            }
        }
    }

    bail!("unable to decode base64 payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_juicity_ok() {
        let parsed = import_share_link("juicity://u:p@example.com:443?sni=example.com").unwrap();
        assert_eq!(parsed.protocol, ProxyProtocol::Juicity);
        assert_eq!(parsed.server, "example.com");
        assert_eq!(parsed.server_port, 443);
        assert_eq!(parsed.uuid, "u");
        assert_eq!(parsed.sni, Some("example.com".to_string()));
    }

    #[test]
    fn parse_ss_sip002_ok() {
        let parsed = import_share_link("ss://YWVzLTI1Ni1nY206cGFzcw@127.0.0.1:8388#demo").unwrap();
        assert_eq!(parsed.protocol, ProxyProtocol::Shadowsocks);
        assert!(parsed.name.contains("demo"));
        assert_eq!(parsed.server, "127.0.0.1");
    }

    #[test]
    fn parse_ss_legacy_ok() {
        let parsed =
            import_share_link("ss://YWVzLTI1Ni1nY206cGFzc0AxMjcuMC4wLjE6ODM4OA==").unwrap();
        assert_eq!(parsed.protocol, ProxyProtocol::Shadowsocks);
        assert_eq!(parsed.server, "127.0.0.1");
    }

    #[test]
    fn roundtrip_juicity() {
        let p = ProxyProfile {
            server: "example.com".to_string(),
            server_port: 443,
            uuid: "test-uuid".to_string(),
            password: "test-pass".to_string(),
            sni: Some("example.com".to_string()),
            name: "My Server".to_string(),
            ..Default::default()
        };
        let link = export_share_link(&p).unwrap();
        assert!(link.starts_with("juicity://"));
        let re = import_share_link(&link).unwrap();
        assert_eq!(re.server, "example.com");
        assert_eq!(re.uuid, "test-uuid");
    }

    #[test]
    fn congestion_control_is_normalized_on_import() {
        let parse = |cc: &str| {
            import_share_link(&format!(
                "juicity://u:p@example.com:443?congestion_control={cc}"
            ))
            .unwrap()
            .congestion_control
        };
        assert_eq!(parse("NewReno").as_deref(), Some("new_reno"));
        assert_eq!(parse("BBR").as_deref(), Some("bbr"));
        assert_eq!(parse("vegas"), None);
    }

    #[test]
    fn roundtrip_juicity_pinned_hash_and_congestion_control() {
        let link = "juicity://u:p@example.com:443?sni=example.com&congestion_control=cubic\
                    &pinned_certchain_sha256=aGFzaA%3D%3D#Tokyo";
        let parsed = import_share_link(link).unwrap();
        assert_eq!(parsed.congestion_control.as_deref(), Some("cubic"));
        assert_eq!(parsed.pinned_certchain_sha256.as_deref(), Some("aGFzaA=="));
        let mut profile = ProxyProfile::default();
        parsed.apply_to(&mut profile);
        let exported = export_share_link(&profile).unwrap();
        assert!(exported.contains("congestion_control=cubic"), "{exported}");
        assert!(
            exported.contains("pinned_certchain_sha256=aGFzaA%3D%3D"),
            "{exported}"
        );
        let mut again = ProxyProfile::default();
        import_share_link(&exported).unwrap().apply_to(&mut again);
        assert_eq!(again, profile);
    }
}
