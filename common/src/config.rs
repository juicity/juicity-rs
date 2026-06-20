use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Main configuration structure matching juicity's JSON config format
///
/// # Field grouping
/// - **Client fields**: server, uuid, password, sni, allow_insecure,
///   pinned_certchain_sha256, protect_path, forward, fwmark
/// - **Server fields**: users, certificate, private_key,
///   send_through, dialer_link, disable_outbound_udp443
/// - **Common fields**: listen, congestion_control, log_level
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // ── Client fields ──
    pub server: String,
    pub uuid: String,
    pub password: String,
    pub sni: String,
    pub allow_insecure: bool,
    pub pinned_certchain_sha256: String,
    /// Path to the protect_path socket (compatible with Go version)
    pub protect_path: String,
    pub forward: HashMap<String, String>,
    /// fwmark (Linux only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fwmark: Option<u32>,

    // ── Server fields ──
    pub users: HashMap<String, String>,
    pub certificate: String,
    pub private_key: String,
    pub send_through: String,
    pub dialer_link: String,
    pub disable_outbound_udp443: bool,

    // ── Common fields ──
    pub listen: String,
    pub congestion_control: String,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            uuid: String::new(),
            password: String::new(),
            sni: String::new(),
            allow_insecure: false,
            pinned_certchain_sha256: String::new(),
            protect_path: String::new(),
            forward: HashMap::new(),
            fwmark: None,
            users: HashMap::new(),
            certificate: String::new(),
            private_key: String::new(),
            send_through: String::new(),
            dialer_link: String::new(),
            disable_outbound_udp443: false,
            listen: String::new(),
            congestion_control: "bbr".to_string(),
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    /// Read config from a JSON file
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Validate config for client run
    pub fn validate_for_client(&self) -> anyhow::Result<()> {
        if self.server.is_empty() {
            anyhow::bail!("'server' is required");
        }
        if !self.server.contains(':') {
            anyhow::bail!("'server' must be in host:port format");
        }
        if self.uuid.is_empty() {
            anyhow::bail!("'uuid' is required");
        }
        // Validate UUID format
        uuid::Uuid::parse_str(&self.uuid)
            .map_err(|e| anyhow::anyhow!("invalid uuid '{}': {}", self.uuid, e))?;
        if self.password.is_empty() {
            anyhow::bail!("'password' is required");
        }
        if self.listen.is_empty() && self.forward.is_empty() {
            anyhow::bail!("'listen' or 'forward' is required");
        }
        if !self.listen.is_empty() && !self.listen.contains(':') {
            anyhow::bail!("'listen' must be in host:port format");
        }
        Ok(())
    }

    /// Validate config for server run
    pub fn validate_for_server(&self) -> anyhow::Result<()> {
        if self.listen.is_empty() {
            anyhow::bail!("'listen' is required");
        }
        if !self.listen.contains(':') {
            anyhow::bail!("'listen' must be in host:port format");
        }
        if self.users.is_empty() {
            anyhow::bail!("'users' is required");
        }
        for (id, pw) in &self.users {
            uuid::Uuid::parse_str(id)
                .map_err(|e| anyhow::anyhow!("invalid user uuid '{}': {}", id, e))?;
            if pw.is_empty() {
                anyhow::bail!("password for user '{}' is required", id);
            }
        }
        if self.certificate.is_empty() {
            anyhow::bail!("'certificate' is required");
        }
        if !std::path::Path::new(&self.certificate).exists() {
            anyhow::bail!("certificate file '{}' not found", self.certificate);
        }
        if self.private_key.is_empty() {
            anyhow::bail!("'private_key' is required");
        }
        if !std::path::Path::new(&self.private_key).exists() {
            anyhow::bail!("private key file '{}' not found", self.private_key);
        }
        Ok(())
    }

    /// Serialize server-relevant fields to a pretty-printed JSON string.
    pub fn to_server_json(&self) -> anyhow::Result<String> {
        let mut map = serde_json::Map::new();
        map.insert("listen".into(), serde_json::Value::String(self.listen.clone()));
        map.insert("users".into(), serde_json::to_value(&self.users)?);
        map.insert("certificate".into(), serde_json::Value::String(self.certificate.clone()));
        map.insert("private_key".into(), serde_json::Value::String(self.private_key.clone()));
        map.insert("congestion_control".into(), serde_json::Value::String(self.congestion_control.clone()));
        map.insert("log_level".into(), serde_json::Value::String(self.log_level.clone()));
        if let Some(fwmark) = self.fwmark {
            map.insert("fwmark".into(), serde_json::Value::Number(fwmark.into()));
        }
        if !self.send_through.is_empty() {
            map.insert("send_through".into(), serde_json::Value::String(self.send_through.clone()));
        }
        if !self.dialer_link.is_empty() {
            map.insert("dialer_link".into(), serde_json::Value::String(self.dialer_link.clone()));
        }
        if self.disable_outbound_udp443 {
            map.insert("disable_outbound_udp443".into(), serde_json::Value::Bool(true));
        }
        Ok(serde_json::to_string_pretty(&serde_json::Value::Object(map))?)
    }

    /// Serialize client-relevant fields to a pretty-printed JSON string.
    /// Used when exporting from an existing client config (fields kept as-is).
    pub fn to_client_json(&self) -> anyhow::Result<String> {
        let mut map = serde_json::Map::new();
        map.insert("server".into(), serde_json::Value::String(self.server.clone()));
        map.insert("uuid".into(), serde_json::Value::String(self.uuid.clone()));
        map.insert("password".into(), serde_json::Value::String(self.password.clone()));
        if !self.sni.is_empty() {
            map.insert("sni".into(), serde_json::Value::String(self.sni.clone()));
        }
        if self.allow_insecure {
            map.insert("allow_insecure".into(), serde_json::Value::Bool(true));
        }
        if !self.pinned_certchain_sha256.is_empty() {
            map.insert("pinned_certchain_sha256".into(), serde_json::Value::String(self.pinned_certchain_sha256.clone()));
        }
        map.insert("congestion_control".into(), serde_json::Value::String(self.congestion_control.clone()));
        if !self.listen.is_empty() {
            map.insert("listen".into(), serde_json::Value::String(self.listen.clone()));
        }
        if !self.forward.is_empty() {
            map.insert("forward".into(), serde_json::to_value(&self.forward)?);
        }
        if let Some(fwmark) = self.fwmark {
            map.insert("fwmark".into(), serde_json::Value::Number(fwmark.into()));
        }
        map.insert("log_level".into(), serde_json::Value::String(self.log_level.clone()));
        Ok(serde_json::to_string_pretty(&serde_json::Value::Object(map))?)
    }

    /// Derive a client config JSON from a server config.
    ///
    /// The first entry in `users` is used as `uuid`/`password`.
    /// `server` is set to the server's `listen` address.
    /// `listen` is set to `[::]:<socks_port>` (default 1080).
    ///
    /// `[::]` 是 IPv6 通配地址。在 Linux 上默认禁用 `IPV6_V6ONLY`，
    /// 因此 `[::]` 启用双栈（dual-stack），可同时接受 IPv4 和 IPv6 连接。
    pub fn to_client_json_from_server(&self, socks_port: u16) -> anyhow::Result<String> {
        let (uuid, password) = self
            .users
            .iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no users defined in server config"))?;

        let mut map = serde_json::Map::new();
        map.insert("server".into(), serde_json::Value::String(self.listen.clone()));
        map.insert("uuid".into(), serde_json::Value::String(uuid.clone()));
        map.insert("password".into(), serde_json::Value::String(password.clone()));
        if !self.sni.is_empty() {
            map.insert("sni".into(), serde_json::Value::String(self.sni.clone()));
        }
        if self.allow_insecure {
            map.insert("allow_insecure".into(), serde_json::Value::Bool(true));
        }
        if !self.pinned_certchain_sha256.is_empty() {
            map.insert("pinned_certchain_sha256".into(), serde_json::Value::String(self.pinned_certchain_sha256.clone()));
        }
        map.insert("congestion_control".into(), serde_json::Value::String(self.congestion_control.clone()));
        // 使用 [::]（IPv6 通配地址）；Linux 上默认禁用 IPV6_V6ONLY，
        // 因此 [::] 启用双栈，可同时接受 IPv4 和 IPv6 连接。
        map.insert("listen".into(), serde_json::Value::String(format!("[::]:{}", socks_port)));
        if let Some(fwmark) = self.fwmark {
            map.insert("fwmark".into(), serde_json::Value::Number(fwmark.into()));
        }
        map.insert("log_level".into(), serde_json::Value::String(self.log_level.clone()));
        Ok(serde_json::to_string_pretty(&serde_json::Value::Object(map))?)
    }
}
