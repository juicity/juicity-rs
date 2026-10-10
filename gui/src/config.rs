use anyhow::Context;
use directories::ProjectDirs;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// SHA-256 of the server certificate chain (hex or base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_certchain_sha256: Option<String>,
    /// `bbr` (default), `cubic` or `new_reno`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub congestion_control: Option<String>,

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

/// Canonical congestion control name; `None` for unknown values, which the
/// core runs as BBR.
pub fn normalize_congestion_control(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bbr" => Some("bbr".to_string()),
        "cubic" => Some("cubic".to_string()),
        "newreno" | "new_reno" => Some("new_reno".to_string()),
        _ => None,
    }
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
            pinned_certchain_sha256: None,
            congestion_control: None,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Single local inbound shared by every protocol.
    ///
    /// It speaks both SOCKS5 and HTTP proxy on the same port (a "mixed"
    /// inbound) by inspecting the first byte of each connection, so the OS
    /// proxy settings, PAC and the core all point at one address.
    ///
    /// The `socks_listen` alias keeps configs written before the mixed
    /// inbound existed loading unchanged.
    #[serde(alias = "socks_listen")]
    pub mixed_listen: String,
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
            mixed_listen: "127.0.0.1:1080".to_string(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LanguagePreference {
    #[default]
    FollowSystem,
    En,
    ZhCn,
    ZhTw,
    Ru,
}

impl LanguagePreference {
    pub fn resolve(self) -> crate::i18n::UiLang {
        use crate::i18n::{self, UiLang};
        match self {
            Self::FollowSystem => i18n::detect(),
            Self::En => UiLang::En,
            Self::ZhCn => UiLang::ZhCn,
            Self::ZhTw => UiLang::ZhTw,
            Self::Ru => UiLang::Ru,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeState {
    /// Saved as `runtime.json`'s `language`; missing fields follow the system locale.
    /// Changes apply to the UI immediately, without restarting the proxy.
    pub language: LanguagePreference,
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
            language: LanguagePreference::default(),
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
        Ok(Self::in_dir(project_dirs.config_dir().to_path_buf()))
    }

    /// Paths of the config files inside `config_dir`.
    pub fn in_dir(config_dir: PathBuf) -> Self {
        Self {
            app_json: config_dir.join("app.json"),
            profiles_json: config_dir.join("profiles.json"),
            runtime_json: config_dir.join("runtime.json"),
            config_dir,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Storage {
    paths: ConfigPaths,
}

impl Storage {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_paths(ConfigPaths::discover()?)
    }

    /// Storage rooted at an explicit directory (used by tests).
    #[cfg(test)]
    pub fn with_dir(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Self::with_paths(ConfigPaths::in_dir(dir.into()))
    }

    fn with_paths(paths: ConfigPaths) -> anyhow::Result<Self> {
        fs::create_dir_all(&paths.config_dir)
            .with_context(|| format!("failed to create {}", paths.config_dir.display()))?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn save_runtime_state(&self, value: &RuntimeState) -> anyhow::Result<()> {
        self.save_pretty_json(&self.paths.runtime_json, value)
    }

    /// Load `path` (default when missing) and also return the bytes that were
    /// parsed, so callers can track the exact content they loaded.
    pub fn load_with_bytes<T>(&self, path: &Path) -> anyhow::Result<(T, Option<Vec<u8>>)>
    where
        T: DeserializeOwned + Default,
    {
        if !path.exists() {
            return Ok((T::default(), None));
        }

        let content =
            fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let value = serde_json::from_slice::<T>(&content)
            .with_context(|| format!("invalid json in {}", path.display()))?;
        Ok((value, Some(content)))
    }

    fn save_pretty_json<T>(&self, path: &Path, value: &T) -> anyhow::Result<()>
    where
        T: Serialize,
    {
        self.write_atomic(path, &serde_json::to_vec_pretty(value)?)
    }

    /// Replace `path` with `payload` via a synced temp file and a rename.
    pub fn write_atomic(&self, path: &Path, payload: &[u8]) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let tmp_path = path.with_extension("tmp");

        {
            let mut file = fs::File::create(&tmp_path)
                .with_context(|| format!("failed to create {}", tmp_path.display()))?;
            file.write_all(payload)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_round_trips_in_runtime_config() {
        for language in [
            LanguagePreference::FollowSystem,
            LanguagePreference::En,
            LanguagePreference::ZhCn,
            LanguagePreference::ZhTw,
            LanguagePreference::Ru,
        ] {
            let runtime = RuntimeState {
                language,
                ..Default::default()
            };
            let saved = serde_json::to_value(&runtime).unwrap();
            let restored: RuntimeState = serde_json::from_value(saved.clone()).unwrap();
            assert_eq!(restored.language, language);
            if language == LanguagePreference::Ru {
                assert_eq!(saved["language"], "ru");
            }
        }
    }

    #[test]
    fn old_runtime_config_follows_system_language() {
        let runtime: RuntimeState = serde_json::from_str(
            r#"{"auto_start":true,"hide_window_on_startup":true,"startup_connection_state":"on"}"#,
        )
        .unwrap();
        assert_eq!(runtime.language, LanguagePreference::FollowSystem);
        assert!(runtime.auto_start);
        assert!(runtime.hide_window_on_startup);
        assert_eq!(runtime.startup_connection_state, StartupConnectionState::On);
    }

    #[test]
    fn profiles_without_new_fields_load_unchanged() {
        let store: ProfileStore = serde_json::from_str(
            r#"{ "profiles": [{ "name": "Tokyo 01", "protocol": "juicity",
                "server": "tokyo.example.com", "server_port": 443, "password": "p",
                "uuid": "u", "sni": "front.example.com", "allow_insecure": true,
                "method": "chacha20-ietf-poly1305", "plugin": null, "plugin_opts": null,
                "plugin_args": null, "timeout": 5, "group": "JP" }] }"#,
        )
        .unwrap();
        let p = &store.profiles[0];
        assert_eq!(p.name, "Tokyo 01");
        assert_eq!(p.sni.as_deref(), Some("front.example.com"));
        assert!(p.allow_insecure);
        assert_eq!(p.group.as_deref(), Some("JP"));
        assert_eq!(p.pinned_certchain_sha256, None);
        assert_eq!(p.congestion_control, None);
        // Unset fields are not written, so older readers see the same file.
        let saved = serde_json::to_string(&store).unwrap();
        assert!(!saved.contains("pinned_certchain_sha256"));
        assert!(!saved.contains("congestion_control"));
    }

    #[test]
    fn congestion_control_is_normalized() {
        assert_eq!(
            normalize_congestion_control(" BBR ").as_deref(),
            Some("bbr")
        );
        assert_eq!(
            normalize_congestion_control("Cubic").as_deref(),
            Some("cubic")
        );
        assert_eq!(
            normalize_congestion_control("NewReno").as_deref(),
            Some("new_reno")
        );
        assert_eq!(
            normalize_congestion_control("new_reno").as_deref(),
            Some("new_reno")
        );
        assert_eq!(normalize_congestion_control("vegas"), None);
        assert_eq!(normalize_congestion_control(""), None);
    }

    #[test]
    fn mixed_listen_defaults_to_1080() {
        assert_eq!(AppConfig::default().mixed_listen, "127.0.0.1:1080");
    }

    #[test]
    fn legacy_socks_listen_field_is_still_accepted() {
        let cfg: AppConfig = serde_json::from_str(r#"{"socks_listen":"127.0.0.1:2080"}"#).unwrap();
        assert_eq!(cfg.mixed_listen, "127.0.0.1:2080");
    }

    #[test]
    fn legacy_http_listen_field_is_ignored() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"socks_listen":"127.0.0.1:2080","http_listen":"127.0.0.1:2081"}"#,
        )
        .unwrap();
        assert_eq!(cfg.mixed_listen, "127.0.0.1:2080");
    }
}
