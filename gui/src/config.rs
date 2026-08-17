use anyhow::Context;
use directories::ProjectDirs;
use rust_i18n::t;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Shadowsocks encryption methods in display order (index maps to dropdown position).
/// Grouped as: AEAD-2022 → AEAD → no-op → deprecated stream ciphers.
pub const SS_METHODS: &[&str] = &[
    // ── AEAD 2022 (SIP022, recommended) ──────────────────────────────────
    "2022-blake3-aes-256-gcm",
    "2022-blake3-aes-128-gcm",
    "2022-blake3-chacha20-poly1305",
    "2022-blake3-chacha8-poly1305",
    // ── AEAD ciphers ─────────────────────────────────────────────────────
    "chacha20-ietf-poly1305",
    "xchacha20-ietf-poly1305",
    "aes-256-gcm",
    "aes-128-gcm",
    // ── No encryption ────────────────────────────────────────────────────
    "none",
    "plain",
    // ── Stream ciphers (deprecated, require stream-cipher feature) ───────
    "aes-256-cfb",
    "aes-192-cfb",
    "aes-128-cfb",
    "aes-256-ctr",
    "aes-192-ctr",
    "aes-128-ctr",
    "camellia-256-cfb",
    "camellia-192-cfb",
    "camellia-128-cfb",
    "rc4-md5",
    "chacha20-ietf",
    "table",
];

pub fn method_to_index(method: &str) -> u32 {
    SS_METHODS.iter().position(|m| *m == method).unwrap_or(0) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyProtocol {
    #[default]
    Juicity,
    Shadowsocks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SystemProxyMode {
    #[default]
    Disable,
    Pac,
    Global,
}

impl SystemProxyMode {
    pub fn label(self) -> String {
        match self {
            SystemProxyMode::Disable => t!("proxy.disable").to_string(),
            SystemProxyMode::Pac => t!("proxy.pac").to_string(),
            SystemProxyMode::Global => t!("proxy.global").to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PacMode {
    #[default]
    Local,
    Online,
}

/// Which rule-set to use when generating the local PAC file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PacRuleMode {
    /// Bypass China-registered domains directly; route everything else via proxy.
    #[default]
    BypassChina,
    /// Only route domains on the GFW block-list via proxy; everything else is direct.
    ProxyGfw,
}

impl PacRuleMode {
    pub fn label(self) -> String {
        match self {
            PacRuleMode::BypassChina => t!("pac.bypass_china").to_string(),
            PacRuleMode::ProxyGfw => t!("pac.gfw_only").to_string(),
        }
    }
}

impl ProxyProtocol {
    pub fn label(self) -> String {
        match self {
            ProxyProtocol::Juicity => t!("protocol.juicity").to_string(),
            ProxyProtocol::Shadowsocks => t!("protocol.shadowsocks").to_string(),
        }
    }

    pub fn from_index(idx: u32) -> Self {
        match idx {
            1 => ProxyProtocol::Shadowsocks,
            _ => ProxyProtocol::Juicity,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            ProxyProtocol::Juicity => 0,
            ProxyProtocol::Shadowsocks => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyProfile {
    /// Remarks / display name shown in the server list.
    pub name: String,
    pub protocol: ProxyProtocol,

    // ── Common connection fields ──────────────────────────────────────────
    pub server: String,
    pub server_port: u16,
    pub password: String,

    // ── Juicity-specific ─────────────────────────────────────────────────
    pub uuid: String,
    pub sni: Option<String>,
    pub allow_insecure: bool,

    // ── Shadowsocks-specific ─────────────────────────────────────────────
    pub method: String,
    pub plugin: Option<String>,
    pub plugin_opts: Option<String>,
    pub plugin_args: Option<String>,

    // ── Common metadata ───────────────────────────────────────────────────
    pub timeout: u32,
    pub group: Option<String>,

    // ── Legacy compat fields (kept so old profiles.json still loads) ──────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl ProxyProfile {
    /// Returns the label shown in the server list.
    /// Mirrors shadowsocks-windows: shows remarks if set, otherwise `host:port`.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() && self.name != "New Server" {
            self.name.clone()
        } else if !self.server.is_empty() {
            format!("{}:{}", self.server, self.server_port)
        } else {
            "New Server".to_string()
        }
    }
}

impl Default for ProxyProfile {
    fn default() -> Self {
        Self {
            name: "New Server".to_string(),
            protocol: ProxyProtocol::Juicity,
            server: String::new(),
            server_port: 443,
            password: String::new(),
            uuid: String::new(),
            sni: None,
            allow_insecure: false,
            method: "chacha20-ietf-poly1305".to_string(),
            plugin: None,
            plugin_opts: None,
            plugin_args: None,
            timeout: 5,
            group: None,
            config_path: None,
            link: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub juicity_client_path: Option<PathBuf>,
    pub ss_local_path: Option<PathBuf>,
    pub socks_listen: String,
    pub http_listen: String,
    pub system_proxy_mode: SystemProxyMode,
    pub pac_mode: PacMode,
    pub pac_rule_mode: PacRuleMode,
    /// Address the local PAC HTTP server listens on.
    pub pac_listen: String,
    pub online_pac_url: Option<String>,
    /// URL for the "bypass / direct" domain list (Loyalsoldier direct-list.txt or custom mirror).
    pub pac_direct_url: String,
    /// URL for the "proxy / GFW" domain list (Loyalsoldier proxy-list.txt or custom mirror).
    pub pac_proxy_url: String,
    /// Auto-update interval in hours; 0 = disabled.
    pub pac_auto_update_hours: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            juicity_client_path: None,
            ss_local_path: None,
            socks_listen: "127.0.0.1:1080".to_string(),
            http_listen: "127.0.0.1:1081".to_string(),
            system_proxy_mode: SystemProxyMode::Disable,
            pac_mode: PacMode::Local,
            pac_rule_mode: PacRuleMode::BypassChina,
            pac_listen: "127.0.0.1:1090".to_string(),
            online_pac_url: None,
            pac_direct_url:
                "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/direct-list.txt"
                    .to_string(),
            pac_proxy_url:
                "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/proxy-list.txt"
                    .to_string(),
            pac_auto_update_hours: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProfileStore {
    pub profiles: Vec<ProxyProfile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StartupConnectionState {
    /// Start with connection off.
    #[default]
    Off,
    /// Start with connection on.
    On,
    /// Restore the connection state from the last session.
    LastState,
}

impl StartupConnectionState {
    pub fn label(self) -> String {
        match self {
            StartupConnectionState::Off => t!("startup_dialog.connection_off").to_string(),
            StartupConnectionState::On => t!("startup_dialog.connection_on").to_string(),
            StartupConnectionState::LastState => t!("startup_dialog.connection_last").to_string(),
        }
    }

    pub fn index(self) -> u32 {
        match self {
            StartupConnectionState::Off => 0,
            StartupConnectionState::On => 1,
            StartupConnectionState::LastState => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeState {
    pub auto_start: bool,
    pub selected_profile: usize,
    pub close_to_tray: bool,
    /// Don't show the main window when the application starts.
    pub hide_window_on_startup: bool,
    /// Connection state to use on startup.
    pub startup_connection_state: StartupConnectionState,
    /// Whether the proxy was running when the application last exited.
    /// Used with StartupConnectionState::LastState to restore the connection.
    pub was_running: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            auto_start: false,
            selected_profile: 0,
            // Minimize to the system tray instead of quitting when the main
            // window is closed — the proxy keeps running in the background.
            close_to_tray: true,
            hide_window_on_startup: false,
            startup_connection_state: StartupConnectionState::default(),
            was_running: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub app_json: PathBuf,
    pub profiles_json: PathBuf,
    pub runtime_json: PathBuf,
}

impl ConfigPaths {
    pub fn discover() -> anyhow::Result<Self> {
        let project_dirs = ProjectDirs::from("io", "juicity", "juicity-gui")
            .context("failed to resolve standard config directory")?;

        let config_dir = project_dirs.config_dir().to_path_buf();
        Ok(Self {
            app_json: config_dir.join("app.json"),
            profiles_json: config_dir.join("profiles.json"),
            runtime_json: config_dir.join("runtime.json"),
            config_dir,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Storage {
    paths: ConfigPaths,
}

impl Storage {
    pub fn new() -> anyhow::Result<Self> {
        let paths = ConfigPaths::discover()?;
        fs::create_dir_all(&paths.config_dir)
            .with_context(|| format!("failed to create {}", paths.config_dir.display()))?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load_app_config(&self) -> anyhow::Result<AppConfig> {
        self.load_or_default(&self.paths.app_json)
    }

    pub fn save_app_config(&self, value: &AppConfig) -> anyhow::Result<()> {
        self.save_pretty_json(&self.paths.app_json, value)
    }

    pub fn load_profiles(&self) -> anyhow::Result<ProfileStore> {
        self.load_or_default(&self.paths.profiles_json)
    }

    pub fn save_profiles(&self, value: &ProfileStore) -> anyhow::Result<()> {
        self.save_pretty_json(&self.paths.profiles_json, value)
    }

    pub fn load_runtime_state(&self) -> anyhow::Result<RuntimeState> {
        self.load_or_default(&self.paths.runtime_json)
    }

    pub fn save_runtime_state(&self, value: &RuntimeState) -> anyhow::Result<()> {
        self.save_pretty_json(&self.paths.runtime_json, value)
    }

    fn load_or_default<T>(&self, path: &Path) -> anyhow::Result<T>
    where
        T: DeserializeOwned + Default,
    {
        if !path.exists() {
            return Ok(T::default());
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value = serde_json::from_str::<T>(&content)
            .with_context(|| format!("invalid json in {}", path.display()))?;
        Ok(value)
    }

    fn save_pretty_json<T>(&self, path: &Path, value: &T) -> anyhow::Result<()>
    where
        T: Serialize,
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let payload = serde_json::to_vec_pretty(value)?;
        let tmp_path = path.with_extension("tmp");

        {
            let mut file = fs::File::create(&tmp_path)
                .with_context(|| format!("failed to create {}", tmp_path.display()))?;
            file.write_all(&payload)
                .with_context(|| format!("failed to write {}", tmp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", tmp_path.display()))?;
        }

        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;

        Ok(())
    }
}
