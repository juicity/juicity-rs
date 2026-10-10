//! Settings page: startup behaviour, local proxy port, PAC sources and the
//! PAC server. Text values are edited in a sheet that holds its own draft.

use super::{Changes, ConfigFile, Controller, Notice};
use crate::config::{
    AppearancePreference, LanguagePreference, PacMode, StartupConnectionState, SystemProxyMode,
};
use crate::state::restart_pac_server;
use crate::util::{format_host_port, split_host_port};
use std::time::Instant;

/// Auto-update presets in hours; 0 (off) and custom values are separate.
pub const INTERVAL_PRESETS: [u32; 5] = [6, 12, 24, 72, 168];

/// A value edited in the settings sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKey {
    MixedPort,
    DirectUrl,
    ProxyUrl,
    UpdateHours,
    PacListen,
    OnlinePacUrl,
}

/// Why a sheet value was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingError {
    Port,
    ListenAddress,
    Url,
    Hours,
}

/// The open sheet and its uncommitted text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingSheet {
    pub key: SettingKey,
    pub draft: String,
    pub error: Option<SettingError>,
}

/// Everything the settings page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub language: LanguagePreference,
    pub appearance: AppearancePreference,
    pub autostart: bool,
    pub hide_on_start: bool,
    pub close_to_tray: bool,
    pub startup: StartupConnectionState,
    pub mixed_port: String,
    pub direct_url: String,
    pub proxy_url: String,
    pub update_hours: u32,
    pub pac_source: PacMode,
    pub pac_listen: String,
    pub online_pac_url: String,
    pub sheet: Option<SettingSheet>,
}

/// Whether `hours` is offered by the update menu without a custom entry.
pub fn is_interval_preset(hours: u32) -> bool {
    hours == 0 || INTERVAL_PRESETS.contains(&hours)
}

/// Short label for a rule list URL: "owner · file" for GitHub, otherwise
/// "host · file".
pub fn url_summary(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let host = parsed.host_str().unwrap_or_default();
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let source = match (host, segments.first()) {
        ("raw.githubusercontent.com" | "github.com", Some(owner)) => owner,
        _ => host,
    };
    match segments.last() {
        Some(file) if !source.is_empty() => format!("{source} · {file}"),
        _ => url.to_string(),
    }
}

/// Validate a sheet value; `Ok` holds the normalized text.
pub fn validate(key: SettingKey, text: &str) -> Result<String, SettingError> {
    let text = text.trim();
    match key {
        SettingKey::MixedPort => match text.parse::<u16>() {
            Ok(port) if port > 0 => Ok(port.to_string()),
            _ => Err(SettingError::Port),
        },
        SettingKey::PacListen => parse_listen(text).ok_or(SettingError::ListenAddress),
        SettingKey::DirectUrl | SettingKey::ProxyUrl => http_url(text),
        // Empty clears the online URL.
        SettingKey::OnlinePacUrl if text.is_empty() => Ok(String::new()),
        SettingKey::OnlinePacUrl => http_url(text),
        SettingKey::UpdateHours => match text.parse::<u32>() {
            Ok(hours) if hours >= 1 => Ok(hours.to_string()),
            _ => Err(SettingError::Hours),
        },
    }
}

/// Normalize `host:port` where host is IPv4, a hostname or a bracketed
/// IPv6 address, and port is 1–65535 without leading zeros.
fn parse_listen(text: &str) -> Option<String> {
    if text.contains(char::is_whitespace) {
        return None;
    }
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let (ip, port) = rest.split_once("]:")?;
            ip.parse::<std::net::Ipv6Addr>().ok()?;
            (format!("[{ip}]"), port)
        }
        None => {
            let (host, port) = text.rsplit_once(':')?;
            let numeric = host.chars().all(|c| c.is_ascii_digit() || c == '.');
            let valid = if numeric {
                host.parse::<std::net::Ipv4Addr>().is_ok()
            } else {
                host.split('.').all(|label| {
                    !label.is_empty()
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
            };
            if !valid {
                return None;
            }
            (host.to_string(), port)
        }
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port = port.parse::<u16>().ok().filter(|port| *port > 0)?;
    Some(format!("{host}:{port}"))
}

fn http_url(text: &str) -> Result<String, SettingError> {
    match url::Url::parse(text) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => {
            Ok(text.to_string())
        }
        _ => Err(SettingError::Url),
    }
}

impl Controller {
    pub fn language(&self) -> LanguagePreference {
        self.gui.runtime.language
    }

    pub fn settings(&self) -> SettingsSnapshot {
        let config = &self.gui.config;
        let runtime = &self.gui.runtime;
        SettingsSnapshot {
            language: runtime.language,
            appearance: runtime.appearance,
            autostart: runtime.auto_start,
            hide_on_start: runtime.hide_window_on_startup,
            close_to_tray: runtime.close_to_tray,
            startup: runtime.startup_connection_state,
            mixed_port: split_host_port(&config.mixed_listen).1.to_string(),
            direct_url: config.pac_direct_url.clone(),
            proxy_url: config.pac_proxy_url.clone(),
            update_hours: config.pac_auto_update_hours,
            pac_source: config.pac_mode,
            pac_listen: config.pac_listen.clone(),
            online_pac_url: config.online_pac_url.clone().unwrap_or_default(),
            sheet: self.sheet.clone(),
        }
    }

    pub fn set_language(&mut self, language: LanguagePreference, now: Instant) -> Changes {
        if self.gui.runtime.language == language {
            return Changes::SETTINGS;
        }
        self.gui.runtime.language = language;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    pub fn set_appearance(&mut self, appearance: AppearancePreference, now: Instant) -> Changes {
        if self.gui.runtime.appearance == appearance {
            return Changes::SETTINGS;
        }
        self.gui.runtime.appearance = appearance;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    /// Write or remove the autostart entry; the switch reverts on failure.
    pub fn set_autostart(&mut self, enabled: bool, now: Instant) -> Changes {
        // The switch is disabled where autostart is not implemented yet.
        if !crate::desktop::autostart::SUPPORTED || self.gui.runtime.auto_start == enabled {
            return Changes::SETTINGS;
        }
        if let Err(err) = self.effects.set_autostart(enabled) {
            let err = err.context("Could not update the autostart entry");
            return Changes::SETTINGS | self.set_notice(Notice::SaveFailed(format!("{err:#}")));
        }
        self.gui.runtime.auto_start = enabled;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    /// Stored now; it takes effect once the tray exists (M4).
    pub fn set_hide_on_start(&mut self, hide: bool, now: Instant) -> Changes {
        if self.gui.runtime.hide_window_on_startup == hide {
            return Changes::SETTINGS;
        }
        self.gui.runtime.hide_window_on_startup = hide;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    /// Stored now; closing the window quits until the tray exists (M4).
    pub fn set_close_to_tray(&mut self, close_to_tray: bool, now: Instant) -> Changes {
        if self.gui.runtime.close_to_tray == close_to_tray {
            return Changes::SETTINGS;
        }
        self.gui.runtime.close_to_tray = close_to_tray;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    pub fn set_startup_connection(
        &mut self,
        state: StartupConnectionState,
        now: Instant,
    ) -> Changes {
        if self.gui.runtime.startup_connection_state == state {
            return Changes::SETTINGS;
        }
        self.gui.runtime.startup_connection_state = state;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::Runtime, now)
    }

    /// Connect on start when the startup setting asks for it.
    pub(super) fn startup_connection(&mut self, now: Instant) -> Changes {
        let runtime = &self.gui.runtime;
        let start = match runtime.startup_connection_state {
            StartupConnectionState::Off => false,
            StartupConnectionState::On => true,
            StartupConnectionState::LastState => runtime.was_running,
        };
        if start {
            self.start_active(now)
        } else {
            Changes::NONE
        }
    }

    /// Local/Online: which PAC URL the system proxy points at.
    pub fn set_pac_source(&mut self, mode: PacMode, now: Instant) -> Changes {
        if self.gui.config.pac_mode == mode {
            return Changes::SETTINGS;
        }
        self.gui.config.pac_mode = mode;
        let changes = Changes::OVERVIEW | Changes::SETTINGS | self.mark_dirty(ConfigFile::App, now);
        changes | self.reapply_pac_proxy()
    }

    /// Pick an auto-update interval from the menu (0 = off).
    pub fn set_update_hours(&mut self, hours: u32, now: Instant) -> Changes {
        if self.gui.config.pac_auto_update_hours == hours {
            return Changes::SETTINGS;
        }
        self.gui.config.pac_auto_update_hours = hours;
        Changes::SETTINGS | self.mark_dirty(ConfigFile::App, now)
    }

    /// Open the sheet for `key` with the current value as its draft.
    pub fn open_setting(&mut self, key: SettingKey) -> Changes {
        let config = &self.gui.config;
        let draft = match key {
            SettingKey::MixedPort => split_host_port(&config.mixed_listen).1.to_string(),
            SettingKey::DirectUrl => config.pac_direct_url.clone(),
            SettingKey::ProxyUrl => config.pac_proxy_url.clone(),
            // Off has no hours to edit; start from the default preset.
            SettingKey::UpdateHours => match config.pac_auto_update_hours {
                0 => "24".to_string(),
                hours => hours.to_string(),
            },
            SettingKey::PacListen => config.pac_listen.clone(),
            // The online URL row is disabled in local mode.
            SettingKey::OnlinePacUrl if config.pac_mode != PacMode::Online => {
                return Changes::NONE;
            }
            SettingKey::OnlinePacUrl => config.online_pac_url.clone().unwrap_or_default(),
        };
        self.sheet = Some(SettingSheet {
            key,
            draft,
            error: None,
        });
        Changes::SETTINGS
    }

    /// Update the sheet draft and its inline error.
    pub fn edit_setting(&mut self, text: &str) -> Changes {
        let Some(sheet) = &mut self.sheet else {
            return Changes::NONE;
        };
        sheet.draft = text.to_string();
        sheet.error = validate(sheet.key, text).err();
        Changes::SETTINGS
    }

    /// Cancel, Esc or the scrim: drop the draft.
    pub fn cancel_setting(&mut self) -> Changes {
        if self.sheet.take().is_some() {
            Changes::SETTINGS
        } else {
            Changes::NONE
        }
    }

    /// Done: validate, apply and close; an invalid draft keeps the sheet open.
    pub fn commit_setting(&mut self, now: Instant) -> Changes {
        let Some(sheet) = &mut self.sheet else {
            return Changes::NONE;
        };
        let value = match validate(sheet.key, &sheet.draft) {
            Ok(value) => value,
            Err(error) => {
                sheet.error = Some(error);
                return Changes::SETTINGS;
            }
        };
        let key = sheet.key;
        self.sheet = None;
        let config = &mut self.gui.config;
        let unchanged = match key {
            SettingKey::MixedPort => {
                let port = value.parse().expect("validated port");
                let listen = format_host_port(split_host_port(&config.mixed_listen).0, port);
                if listen == config.mixed_listen {
                    true
                } else {
                    config.mixed_listen = listen;
                    return Changes::SETTINGS
                        | self.mark_dirty(ConfigFile::App, now)
                        | self.apply_listen_change(now);
                }
            }
            SettingKey::DirectUrl => replace(&mut config.pac_direct_url, value),
            SettingKey::ProxyUrl => replace(&mut config.pac_proxy_url, value),
            SettingKey::UpdateHours => {
                let hours = value.parse().expect("validated hours");
                let same = config.pac_auto_update_hours == hours;
                config.pac_auto_update_hours = hours;
                same
            }
            SettingKey::PacListen => {
                if config.pac_listen == value {
                    true
                } else {
                    return Changes::SETTINGS | self.change_pac_listen(value, now);
                }
            }
            SettingKey::OnlinePacUrl => {
                let url = (!value.is_empty()).then_some(value);
                if config.online_pac_url == url {
                    true
                } else {
                    config.online_pac_url = url;
                    return Changes::OVERVIEW
                        | Changes::SETTINGS
                        | self.mark_dirty(ConfigFile::App, now)
                        | self.reapply_pac_proxy();
                }
            }
        };
        if unchanged {
            Changes::SETTINGS
        } else {
            Changes::SETTINGS | self.mark_dirty(ConfigFile::App, now)
        }
    }

    /// The local mixed port changed: regenerate the PAC, re-point the system
    /// proxy and restart a running core.
    pub(super) fn apply_listen_change(&mut self, now: Instant) -> Changes {
        let mut changes = Changes::OVERVIEW;
        if let Err(err) = restart_pac_server(&mut self.gui, false) {
            changes |= self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
        }
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            if let Err(err) = self.effects.apply_system_proxy(&self.gui.config) {
                changes |= self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
            }
        }
        if self.connected {
            changes |= self.start_active(now);
        }
        changes
    }

    /// Serve from a new PAC address. The address is committed only once the
    /// new server is bound; on failure the previous address and server stay,
    /// so the same value can be retried.
    fn change_pac_listen(&mut self, listen: String, now: Instant) -> Changes {
        let previous = std::mem::replace(&mut self.gui.config.pac_listen, listen);
        if let Err(err) = restart_pac_server(&mut self.gui, true) {
            self.gui.config.pac_listen = previous;
            return self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
        }
        Changes::OVERVIEW | self.mark_dirty(ConfigFile::App, now) | self.reapply_pac_proxy()
    }

    /// Re-point the OS proxy when it is in PAC mode.
    fn reapply_pac_proxy(&mut self) -> Changes {
        if self.gui.config.system_proxy_mode != SystemProxyMode::Pac {
            return Changes::NONE;
        }
        match self.effects.apply_system_proxy(&self.gui.config) {
            Ok(()) => Changes::NONE,
            Err(err) => self.set_notice(Notice::ProxyFailed(format!("{err:#}"))),
        }
    }
}

/// Store `value`; `true` when it was already there.
fn replace(slot: &mut String, value: String) -> bool {
    if *slot == value {
        true
    } else {
        *slot = value;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn read(dir: &std::path::Path, file: &str) -> String {
        std::fs::read_to_string(dir.join(file)).unwrap_or_default()
    }

    fn pac_content(c: &Controller) -> String {
        c.gui
            .pac_server
            .as_ref()
            .unwrap()
            .content
            .lock()
            .unwrap()
            .clone()
    }

    #[test]
    fn snapshot_maps_config_and_runtime() {
        let dir = temp_dir("settings-map");
        std::fs::write(
            dir.join("app.json"),
            r#"{ "pac_listen": "127.0.0.1:0", "mixed_listen": "[::1]:2080",
                "pac_mode": "online", "online_pac_url": "https://example.com/proxy.pac",
                "pac_auto_update_hours": 48 }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("runtime.json"),
            r#"{ "auto_start": true, "close_to_tray": false,
                "hide_window_on_startup": true, "startup_connection_state": "last_state" }"#,
        )
        .unwrap();
        let (c, _) = controller(&dir);
        let s = c.settings();
        assert!(s.autostart && s.hide_on_start && !s.close_to_tray);
        assert_eq!(s.startup, StartupConnectionState::LastState);
        assert_eq!(s.mixed_port, "2080");
        assert_eq!(s.update_hours, 48);
        assert_eq!(s.pac_source, PacMode::Online);
        assert_eq!(s.online_pac_url, "https://example.com/proxy.pac");
        assert_eq!(s.sheet, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn switches_and_menus_persist() {
        let dir = temp_dir("settings-switches");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        let _ = c.set_hide_on_start(true, now);
        let _ = c.set_close_to_tray(false, now);
        let _ = c.set_startup_connection(StartupConnectionState::On, now);
        let _ = c.set_autostart(true, now);
        assert_eq!(fake.0.borrow().autostart, [true]);
        let _ = c.set_update_hours(72, now);
        c.shutdown();
        let runtime = read(&dir, "runtime.json");
        assert!(runtime.contains("\"auto_start\": true"), "{runtime}");
        assert!(
            runtime.contains("\"hide_window_on_startup\": true"),
            "{runtime}"
        );
        assert!(runtime.contains("\"close_to_tray\": false"), "{runtime}");
        assert!(
            runtime.contains("\"startup_connection_state\": \"on\""),
            "{runtime}"
        );
        assert!(read(&dir, "app.json").contains("\"pac_auto_update_hours\": 72"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn failed_autostart_keeps_the_switch_off() {
        let dir = temp_dir("settings-autostart-fail");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().autostart_error = Some("read-only".into());
        let changes = c.set_autostart(true, Instant::now());
        assert!(changes.settings && changes.notice);
        assert!(!c.settings().autostart);
        assert!(matches!(c.notice().0, Notice::SaveFailed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn autostart_toggle_creates_then_removes_the_entry() {
        let dir = temp_dir("settings-autostart");
        let autostart = dir.join("xdg-config/autostart");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().autostart_dir = Some(autostart.clone());
        let entry = autostart.join("io.juicity.gui.desktop");
        let _ = c.set_autostart(true, Instant::now());
        assert!(entry.exists());
        let _ = c.set_autostart(false, Instant::now());
        assert!(!entry.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interval_presets_and_custom_values() {
        for hours in [0, 6, 12, 24, 72, 168] {
            assert!(is_interval_preset(hours), "{hours}");
        }
        for hours in [1, 5, 48, 1000] {
            assert!(!is_interval_preset(hours), "{hours}");
        }
        let dir = temp_dir("settings-interval");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        // Off opens with the default preset, so Done works right away.
        let _ = c.open_setting(SettingKey::UpdateHours);
        assert_eq!(c.settings().sheet.unwrap().draft, "24");
        assert_eq!(c.settings().sheet.unwrap().error, None);
        let _ = c.edit_setting("0");
        assert_eq!(c.settings().sheet.unwrap().error, Some(SettingError::Hours));
        let _ = c.commit_setting(now);
        assert!(
            c.settings().sheet.is_some(),
            "an invalid draft keeps the sheet open"
        );
        let _ = c.edit_setting(" 48 ");
        assert_eq!(c.settings().sheet.unwrap().error, None);
        let _ = c.commit_setting(now);
        assert_eq!(c.settings().update_hours, 48);
        assert!(c.settings().sheet.is_none());
        // A custom value opens as the draft next time.
        let _ = c.open_setting(SettingKey::UpdateHours);
        assert_eq!(c.settings().sheet.unwrap().draft, "48");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_rules() {
        use SettingKey::*;
        assert_eq!(validate(MixedPort, "2080"), Ok("2080".into()));
        for bad in ["0", "65536", "abc", ""] {
            assert_eq!(validate(MixedPort, bad), Err(SettingError::Port), "{bad}");
        }
        assert!(validate(PacListen, "127.0.0.1:1090").is_ok());
        assert!(validate(PacListen, "[::1]:1090").is_ok());
        assert!(validate(PacListen, "localhost:1090").is_ok());
        assert_eq!(
            validate(PacListen, "127.0.0.1:01090"),
            Ok("127.0.0.1:1090".into())
        );
        assert_eq!(
            validate(PacListen, "pac.example-1.lan:80"),
            Ok("pac.example-1.lan:80".into())
        );
        for bad in [
            "::1:1090",
            "[not-an-ip]:1090",
            "a:1:2",
            "127.0.0.1: 1090",
            "999.1.1.1:80",
            "[::1]1090",
        ] {
            assert_eq!(
                validate(PacListen, bad),
                Err(SettingError::ListenAddress),
                "{bad}"
            );
        }
        for bad in [
            "127.0.0.1",
            ":1090",
            "127.0.0.1:0",
            "127.0.0.1:x",
            "127.0.0.1:99999",
        ] {
            assert_eq!(
                validate(PacListen, bad),
                Err(SettingError::ListenAddress),
                "{bad}"
            );
        }
        assert!(validate(DirectUrl, "https://example.com/list.txt").is_ok());
        assert!(validate(ProxyUrl, "http://mirror.example/proxy.txt").is_ok());
        for bad in ["ftp://example.com/a", "example.com/a", "https://"] {
            assert_eq!(validate(DirectUrl, bad), Err(SettingError::Url), "{bad}");
        }
        assert_eq!(validate(OnlinePacUrl, " "), Ok(String::new()));
        assert_eq!(
            validate(OnlinePacUrl, "file:///pac"),
            Err(SettingError::Url)
        );
        assert_eq!(validate(UpdateHours, "1"), Ok("1".into()));
        assert_eq!(validate(UpdateHours, "-3"), Err(SettingError::Hours));
    }

    #[test]
    fn rule_urls_are_summarized() {
        assert_eq!(
            url_summary(
                "https://raw.githubusercontent.com/Loyalsoldier/v2ray-rules-dat/release/direct-list.txt"
            ),
            "Loyalsoldier · direct-list.txt"
        );
        assert_eq!(
            url_summary("https://mirror.example.com/rules/proxy.txt"),
            "mirror.example.com · proxy.txt"
        );
        assert_eq!(url_summary(""), "");
    }

    #[test]
    fn sheet_cancel_discards_and_commit_persists() {
        let dir = temp_dir("settings-sheet");
        let (mut c, _) = controller(&dir);
        let url = "https://mirror.example.com/direct.txt";
        let _ = c.open_setting(SettingKey::DirectUrl);
        let _ = c.edit_setting(url);
        let _ = c.cancel_setting();
        assert_ne!(c.settings().direct_url, url);
        let _ = c.open_setting(SettingKey::DirectUrl);
        let _ = c.edit_setting(url);
        let changes = c.commit_setting(Instant::now());
        assert!(changes.persist);
        assert_eq!(c.settings().direct_url, url);
        c.shutdown();
        assert!(read(&dir, "app.json").contains(url));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_change_regenerates_pac_reapplies_proxy_and_restarts_core() {
        let dir = temp_dir("settings-listen");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        let (_, _) = c.startup(now);
        let _ = c.set_proxy_mode(SystemProxyMode::Pac, now);
        let _ = c.toggle_connection(now);
        assert!(pac_content(&c).contains("127.0.0.1:1080"));
        fake.0.borrow_mut().proxy_modes.clear();
        fake.0.borrow_mut().started.clear();
        let _ = c.open_setting(SettingKey::MixedPort);
        let _ = c.edit_setting("2080");
        let changes = c.commit_setting(now);
        assert!(changes.overview && changes.persist);
        assert_eq!(c.gui.config.mixed_listen, "127.0.0.1:2080");
        assert!(
            pac_content(&c).contains("SOCKS5 127.0.0.1:2080"),
            "{}",
            pac_content(&c)
        );
        assert_eq!(fake.0.borrow().proxy_modes, [SystemProxyMode::Pac]);
        assert_eq!(fake.0.borrow().started, ["Tokyo 01"]);
        assert_eq!(c.overview().local_port, "2080");
        // The same port again changes nothing.
        let _ = c.open_setting(SettingKey::MixedPort);
        let _ = c.commit_setting(now);
        assert_eq!(fake.0.borrow().started.len(), 1);
        c.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_change_while_stopped_does_not_start_the_core() {
        let dir = temp_dir("settings-listen-stopped");
        let (mut c, fake) = controller(&dir);
        let _ = c.open_setting(SettingKey::MixedPort);
        let _ = c.edit_setting("2080");
        let _ = c.commit_setting(Instant::now());
        assert!(fake.0.borrow().started.is_empty());
        assert!(fake.0.borrow().proxy_modes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pac_source_switch_reapplies_and_gates_the_online_url() {
        let dir = temp_dir("settings-source");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        assert_eq!(c.open_setting(SettingKey::OnlinePacUrl), Changes::NONE);
        let _ = c.set_proxy_mode(SystemProxyMode::Pac, now);
        fake.0.borrow_mut().proxy_modes.clear();
        let changes = c.set_pac_source(PacMode::Online, now);
        assert!(changes.overview && changes.settings && changes.persist);
        assert_eq!(fake.0.borrow().proxy_modes, [SystemProxyMode::Pac]);
        let _ = c.open_setting(SettingKey::OnlinePacUrl);
        assert_eq!(c.settings().sheet.unwrap().draft, "");
        let _ = c.edit_setting("https://example.com/proxy.pac");
        let _ = c.commit_setting(now);
        assert_eq!(c.overview().pac_url, "https://example.com/proxy.pac");
        assert_eq!(fake.0.borrow().proxy_modes.len(), 2);
        let _ = c.set_pac_source(PacMode::Local, now);
        assert_eq!(c.overview().pac_url, "http://127.0.0.1:0/pac");
        assert_eq!(c.gui.config.pac_mode, PacMode::Local);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .unwrap()
            .port()
    }

    fn set_pac_listen(c: &mut Controller, value: &str) -> Changes {
        let _ = c.open_setting(SettingKey::PacListen);
        let _ = c.edit_setting(value);
        c.commit_setting(Instant::now())
    }

    #[test]
    fn pac_server_address_round_trip_rebinds_each_port() {
        let dir = temp_dir("settings-pac-listen");
        let (mut c, _) = controller(&dir);
        let (_, _) = c.startup(Instant::now());
        let _ = c.open_setting(SettingKey::PacListen);
        let _ = c.edit_setting("127.0.0.1");
        let _ = c.commit_setting(Instant::now());
        assert_eq!(
            c.settings().sheet.unwrap().error,
            Some(SettingError::ListenAddress)
        );
        let _ = c.cancel_setting();
        let (first, second) = (free_port(), free_port());
        for port in [first, second, first] {
            let changes = set_pac_listen(&mut c, &format!("127.0.0.1:{port} "));
            assert!(
                changes.overview && changes.persist && !changes.notice,
                "{port}"
            );
            assert_eq!(c.gui.config.pac_listen, format!("127.0.0.1:{port}"));
            assert_eq!(c.overview().pac_url, format!("http://127.0.0.1:{port}/pac"));
            assert_eq!(
                c.gui.pac_server.as_ref().unwrap().listen(),
                c.gui.config.pac_listen
            );
        }
        // The previous server released its port.
        assert!(std::net::TcpListener::bind(("127.0.0.1", second)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_pac_bind_keeps_the_previous_address_and_allows_retry() {
        let dir = temp_dir("settings-pac-busy");
        let (mut c, _) = controller(&dir);
        let (_, _) = c.startup(Instant::now());
        let previous = c.gui.config.pac_listen.clone();
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("127.0.0.1:{}", busy.local_addr().unwrap().port());
        let changes = set_pac_listen(&mut c, &target);
        assert!(changes.notice && !changes.persist);
        assert!(matches!(c.notice().0, Notice::ProxyFailed(_)));
        assert_eq!(c.gui.config.pac_listen, previous);
        assert_eq!(c.gui.pac_server.as_ref().unwrap().listen(), previous);
        drop(busy);
        let changes = set_pac_listen(&mut c, &target);
        assert!(
            changes.persist,
            "the same value is retried, not treated as unchanged"
        );
        assert_eq!(c.gui.config.pac_listen, target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_connection_follows_the_setting() {
        for (state, was_running, starts) in [
            ("off", true, false),
            ("on", false, true),
            ("last_state", true, true),
            ("last_state", false, false),
        ] {
            let dir = temp_dir(&format!("settings-startup-{state}-{was_running}"));
            std::fs::write(
                dir.join("runtime.json"),
                format!(
                    r#"{{ "startup_connection_state": "{state}", "was_running": {was_running} }}"#
                ),
            )
            .unwrap();
            let (mut c, fake) = controller(&dir);
            let _ = c.startup(Instant::now());
            assert_eq!(fake.0.borrow().running, starts, "{state} {was_running}");
            c.shutdown();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
