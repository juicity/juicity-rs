//! Plain-Rust application controller for the Slint UI.
//!
//! The controller owns [`GuiState`] plus the core/PAC side effects and never
//! touches Slint types. Every mutation returns [`Changes`] so the bind layer
//! knows which snapshots to push into the UI after the borrow is released.

mod nodes;
mod overview;
mod persist;
mod settings;
mod tray;

pub use nodes::{DraftData, DraftError, DraftField, ListCommand, NodesSnapshot};
pub use overview::{OverviewSnapshot, RuleJob};
pub use persist::ConfigFile;
#[cfg(test)]
use persist::{DEBOUNCE, RETRY};
pub use settings::{is_interval_preset, url_summary, SettingError, SettingKey, SettingsSnapshot};

use crate::config::{AppConfig, ProxyProfile, Storage, SystemProxyMode};
use crate::core::CoreManager;
use crate::state::{restart_pac_server, GuiState};
use crate::validate::RequiredField;
use persist::Persist;
use std::time::Instant;

/// Side effects that leave the process: the proxy core, the OS proxy
/// settings and the clipboard. Tests replace them with a fake.
pub trait Effects {
    fn start_core(
        &mut self,
        core: &mut CoreManager,
        config: &AppConfig,
        profile: &ProxyProfile,
    ) -> anyhow::Result<()>;
    fn stop_core(&mut self, core: &mut CoreManager);
    /// `Some(reason)` when the core stopped unexpectedly.
    fn poll_core(&mut self, core: &mut CoreManager) -> Option<String>;
    fn apply_system_proxy(&mut self, config: &AppConfig) -> anyhow::Result<()>;
    fn copy_text(&mut self, text: &str) -> anyhow::Result<()>;
    fn paste_text(&mut self) -> anyhow::Result<String>;
    /// Enable or disable starting at login.
    fn set_autostart(&mut self, enabled: bool) -> anyhow::Result<()>;
}

/// The real effects: in-process core, `system_proxy.rs` and `arboard`.
#[derive(Default)]
pub struct NativeEffects {
    /// Kept alive because X11 and Wayland clipboards are served by the owner.
    clipboard: Option<arboard::Clipboard>,
}

impl Effects for NativeEffects {
    fn start_core(
        &mut self,
        core: &mut CoreManager,
        config: &AppConfig,
        profile: &ProxyProfile,
    ) -> anyhow::Result<()> {
        core.start_profile(config, profile)
    }

    fn stop_core(&mut self, core: &mut CoreManager) {
        core.stop_and_wait();
    }

    fn poll_core(&mut self, core: &mut CoreManager) -> Option<String> {
        core.poll().unwrap_or_else(|err| Some(format!("{err:#}")))
    }

    fn apply_system_proxy(&mut self, config: &AppConfig) -> anyhow::Result<()> {
        crate::system_proxy::apply_system_proxy(config)
    }

    fn copy_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.clipboard()?.set_text(text)?;
        Ok(())
    }

    fn paste_text(&mut self) -> anyhow::Result<String> {
        Ok(self.clipboard()?.get_text()?)
    }

    fn set_autostart(&mut self, enabled: bool) -> anyhow::Result<()> {
        crate::desktop::autostart::apply(enabled)
    }
}

impl NativeEffects {
    fn clipboard(&mut self) -> anyhow::Result<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new()?);
        }
        Ok(self.clipboard.as_mut().expect("clipboard was just created"))
    }
}

/// Feedback shown in the Banner. Details stay in English.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Notice {
    #[default]
    None,
    Imported,
    ImportFailed(String),
    /// Links added and lines skipped.
    ImportPartial(usize, usize),
    ExportFailed(String),
    /// The editor has invalid fields.
    ExportInvalid,
    LinkCopied,
    StartFailed(String),
    RulesUpdated,
    RulesFailed(String),
    ProxyFailed(String),
    SaveFailed(String),
    CoreExited(String),
    NoNode,
    MissingFields(Vec<RequiredField>),
}

impl Notice {
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::StartFailed(_)
                | Self::RulesFailed(_)
                | Self::ProxyFailed(_)
                | Self::SaveFailed(_)
                | Self::CoreExited(_)
                | Self::NoNode
                | Self::MissingFields(_)
                | Self::ImportFailed(_)
                | Self::ExportFailed(_)
                | Self::ExportInvalid
        )
    }
}

/// What a controller call changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct Changes {
    /// The overview snapshot must be pushed again.
    pub overview: bool,
    /// The Banner must be updated.
    pub notice: bool,
    /// A config file became dirty; (re)arm the save timer.
    pub persist: bool,
    /// The node list, errors and sheet state must be pushed again.
    pub nodes: bool,
    /// The editor draft was replaced (another node selected); its text
    /// fields must be reloaded.
    pub editor: bool,
    /// The settings snapshot (values and the open sheet) must be pushed.
    pub settings: bool,
}

impl Changes {
    pub const NONE: Self = Self {
        overview: false,
        notice: false,
        persist: false,
        nodes: false,
        editor: false,
        settings: false,
    };
    pub const OVERVIEW: Self = Self {
        overview: true,
        ..Self::NONE
    };
    pub const NODES: Self = Self {
        nodes: true,
        ..Self::NONE
    };
    pub const EDITOR: Self = Self {
        nodes: true,
        editor: true,
        ..Self::NONE
    };
    pub const SETTINGS: Self = Self {
        settings: true,
        ..Self::NONE
    };
}

impl std::ops::BitOr for Changes {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self {
            overview: self.overview || other.overview,
            notice: self.notice || other.notice,
            persist: self.persist || other.persist,
            nodes: self.nodes || other.nodes,
            editor: self.editor || other.editor,
            settings: self.settings || other.settings,
        }
    }
}

impl std::ops::BitOrAssign for Changes {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

pub struct Controller {
    gui: GuiState,
    effects: Box<dyn Effects>,
    persist: Persist,
    connected: bool,
    rules_updating: bool,
    notice: Notice,
    notice_seq: u64,
    nodes: nodes::NodesState,
    /// The open settings sheet; its draft is committed only by 完成.
    sheet: Option<settings::SettingSheet>,
}

impl Controller {
    pub fn new(storage: Storage, effects: Box<dyn Effects>) -> Self {
        let (gui, loaded) = GuiState::load_tracked(storage);
        let persist = Persist::new(&loaded);
        let nodes = nodes::NodesState::new(&gui);
        Self {
            gui,
            effects,
            persist,
            connected: false,
            rules_updating: false,
            notice: Notice::None,
            notice_seq: 0,
            nodes,
            sheet: None,
        }
    }

    pub fn notice(&self) -> (Notice, u64) {
        (self.notice.clone(), self.notice_seq)
    }

    fn set_notice(&mut self, notice: Notice) -> Changes {
        if notice.is_error() {
            tracing::warn!("{notice:?}");
        }
        self.notice = notice;
        self.notice_seq += 1;
        Changes {
            notice: true,
            ..Changes::NONE
        }
    }

    pub fn dismiss_notice(&mut self) -> Changes {
        self.set_notice(Notice::None)
    }

    /// Hide an info notice once its timer fires, unless a newer one replaced it.
    pub fn expire_notice(&mut self, seq: u64) -> Changes {
        if seq == self.notice_seq && !self.notice.is_error() {
            self.dismiss_notice()
        } else {
            Changes::NONE
        }
    }

    /// Startup side effects: PAC server, saved system proxy mode, the
    /// startup connection setting and the startup-only rule overdue check.
    pub fn startup(&mut self, now: Instant) -> (Changes, Option<RuleJob>) {
        let mut changes = Changes::OVERVIEW;
        if let Err(err) = restart_pac_server(&mut self.gui, true) {
            tracing::warn!("PAC server failed to start: {err:#}");
        }
        // Start the core first, then re-apply the proxy.
        changes |= self.startup_connection(now);
        // The OS proxy is reset to Disable on exit, so re-apply the saved mode.
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            if let Err(err) = self.effects.apply_system_proxy(&self.gui.config) {
                changes |= self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
            }
        }
        let job = self.rules_overdue().then(|| self.begin_rules()).flatten();
        (changes, job)
    }

    /// Stop the core, restore the OS proxy to Disable, then flush.
    pub fn shutdown(&mut self) {
        self.effects.stop_core(&mut self.gui.core_manager);
        self.connected = false;
        if self.gui.config.system_proxy_mode != SystemProxyMode::Disable {
            let mut config = self.gui.config.clone();
            config.system_proxy_mode = SystemProxyMode::Disable;
            if let Err(err) = self.effects.apply_system_proxy(&config) {
                tracing::warn!("failed to restore system proxy on exit: {err:#}");
            }
        }
        if let Err(err) = self.flush_all(Instant::now()) {
            tracing::warn!("failed to save settings on exit: {err:#}");
        }
    }

    /// Mark a config file dirty; the save timer writes it after [`DEBOUNCE`].
    pub fn mark_dirty(&mut self, file: ConfigFile, now: Instant) -> Changes {
        self.persist.mark(file, now);
        Changes {
            persist: true,
            ..Changes::NONE
        }
    }

    /// Time left until the pending save, if any.
    pub fn save_delay(&self, now: Instant) -> Option<std::time::Duration> {
        self.persist.delay(now)
    }

    /// Write dirty files once the debounce has elapsed.
    pub fn flush_due(&mut self, now: Instant) -> Changes {
        match self.persist.delay(now) {
            None => Changes::NONE,
            Some(delay) if !delay.is_zero() => Changes {
                persist: true,
                ..Changes::NONE
            },
            Some(_) => match self.flush_all(now) {
                Ok(()) => Changes::NONE,
                // The file stays dirty and `flush_all` scheduled a retry.
                Err(err) => {
                    self.set_notice(Notice::SaveFailed(format!("{err:#}")))
                        | Changes {
                            persist: true,
                            ..Changes::NONE
                        }
                }
            },
        }
    }

    /// Write every dirty file now. A failed file stays dirty and is retried
    /// after `RETRY`.
    pub fn flush_all(&mut self, now: Instant) -> anyhow::Result<()> {
        self.persist.flush(&self.gui, now)
    }

    /// Handle a watcher event: ignore our own writes, keep locally dirty
    /// files, reload clean files that changed on disk.
    pub fn on_files_changed(&mut self, now: Instant) -> Changes {
        let old = self.gui.config.clone();
        let edited = self.edited_profile();
        let reloaded = self.persist.reload_changed(&mut self.gui, now);
        let mut changes = Changes {
            persist: self.persist.delay(now).is_some(),
            ..Changes::NONE
        };
        if reloaded.is_empty() {
            return changes;
        }
        tracing::info!("config reloaded from disk: {reloaded:?}");
        self.gui.normalize_selected_index();
        changes |= Changes::OVERVIEW | Changes::SETTINGS | self.after_reload(edited);
        if reloaded.contains(&ConfigFile::App) {
            let config = &self.gui.config;
            let listen_changed = old.pac_listen != config.pac_listen;
            if let Err(err) = restart_pac_server(&mut self.gui, listen_changed) {
                // Do not point the OS at a PAC URL nobody serves.
                return changes | self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
            }
            let config = &self.gui.config;
            let proxy_changed = old.system_proxy_mode != config.system_proxy_mode
                || old.mixed_listen != config.mixed_listen
                || old.pac_listen != config.pac_listen
                || old.pac_mode != config.pac_mode
                || old.online_pac_url != config.online_pac_url;
            if proxy_changed {
                if let Err(err) = self.effects.apply_system_proxy(&self.gui.config) {
                    changes |= self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
                }
            }
        }
        changes
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    #[derive(Default)]
    pub struct FakeState {
        pub start_error: Option<String>,
        pub exit_reason: Option<String>,
        pub running: bool,
        pub proxy_modes: Vec<SystemProxyMode>,
        pub copied: Vec<String>,
        /// Clipboard text returned to an import.
        pub paste: String,
        /// Names of the profiles the core was started with.
        pub started: Vec<String>,
        /// Autostart requests, in order.
        pub autostart: Vec<bool>,
        pub autostart_error: Option<String>,
        /// When set, autostart entries are really written here.
        #[cfg(target_os = "linux")]
        pub autostart_dir: Option<PathBuf>,
    }

    /// Effects that only record what the controller asked for.
    #[derive(Clone, Default)]
    pub struct FakeEffects(pub Rc<RefCell<FakeState>>);

    impl Effects for FakeEffects {
        fn start_core(
            &mut self,
            _core: &mut CoreManager,
            _config: &AppConfig,
            profile: &ProxyProfile,
        ) -> anyhow::Result<()> {
            let mut state = self.0.borrow_mut();
            if let Some(err) = state.start_error.clone() {
                state.running = false;
                anyhow::bail!("{err}");
            }
            state.running = true;
            state.started.push(profile.name.clone());
            Ok(())
        }

        fn stop_core(&mut self, _core: &mut CoreManager) {
            self.0.borrow_mut().running = false;
        }

        fn poll_core(&mut self, _core: &mut CoreManager) -> Option<String> {
            let mut state = self.0.borrow_mut();
            let reason = state.exit_reason.take();
            if reason.is_some() {
                state.running = false;
            }
            reason
        }

        fn apply_system_proxy(&mut self, config: &AppConfig) -> anyhow::Result<()> {
            self.0
                .borrow_mut()
                .proxy_modes
                .push(config.system_proxy_mode);
            Ok(())
        }

        fn copy_text(&mut self, text: &str) -> anyhow::Result<()> {
            self.0.borrow_mut().copied.push(text.to_string());
            Ok(())
        }

        fn paste_text(&mut self) -> anyhow::Result<String> {
            Ok(self.0.borrow().paste.clone())
        }

        fn set_autostart(&mut self, enabled: bool) -> anyhow::Result<()> {
            let mut state = self.0.borrow_mut();
            if let Some(err) = state.autostart_error.clone() {
                anyhow::bail!("{err}");
            }
            state.autostart.push(enabled);
            #[cfg(target_os = "linux")]
            if let Some(dir) = &state.autostart_dir {
                let exe = std::path::Path::new("/usr/bin/juicity-gui");
                return crate::desktop::autostart::apply_in(dir, enabled, exe);
            }
            Ok(())
        }
    }

    /// A fresh config directory under the system temp dir.
    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "juicity-gui-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Keep tests off the real PAC port.
        std::fs::write(dir.join("app.json"), r#"{ "pac_listen": "127.0.0.1:0" }"#).unwrap();
        std::fs::write(
            dir.join("profiles.json"),
            r#"{ "profiles": [{ "name": "Tokyo 01", "server": "tokyo.example.com",
                "server_port": 443, "uuid": "u", "password": "p" }] }"#,
        )
        .unwrap();
        dir
    }

    pub fn controller(dir: &std::path::Path) -> (Controller, FakeEffects) {
        let effects = FakeEffects::default();
        let storage = Storage::with_dir(dir).unwrap();
        (Controller::new(storage, Box::new(effects.clone())), effects)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::config::PacRuleMode;
    use std::time::Duration;

    fn read(dir: &std::path::Path, file: &str) -> String {
        std::fs::read_to_string(dir.join(file)).unwrap_or_default()
    }

    #[test]
    fn change_is_flushed_after_debounce() {
        let dir = temp_dir("debounce");
        let (mut c, _) = controller(&dir);
        let t0 = Instant::now();
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, t0);
        assert!(c.flush_due(t0 + Duration::from_millis(100)).persist);
        assert!(!read(&dir, "app.json").contains("proxy_gfw"));
        assert_eq!(c.flush_due(t0 + DEBOUNCE), Changes::NONE);
        assert!(read(&dir, "app.json").contains("proxy_gfw"));
        assert!(!dir.join("app.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_then_quit_is_saved() {
        let dir = temp_dir("quit");
        let (mut c, _) = controller(&dir);
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, Instant::now());
        c.shutdown();
        assert!(read(&dir, "app.json").contains("proxy_gfw"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_failure_keeps_file_dirty() {
        let dir = temp_dir("fail");
        let (mut c, _) = controller(&dir);
        let t0 = Instant::now();
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, t0);
        // A directory in place of app.json makes the final rename fail.
        std::fs::remove_file(dir.join("app.json")).unwrap();
        std::fs::create_dir(dir.join("app.json")).unwrap();
        let failed_at = t0 + DEBOUNCE;
        let changes = c.flush_due(failed_at);
        assert!(changes.notice);
        assert!(matches!(c.notice().0, Notice::SaveFailed(_)));
        assert!(c.persist.is_dirty(ConfigFile::App));
        // A retry is scheduled after RETRY, not after the short debounce.
        assert!(changes.persist, "the save timer must be re-armed");
        assert_eq!(c.save_delay(failed_at), Some(RETRY));
        std::fs::remove_dir(dir.join("app.json")).unwrap();
        assert!(c.flush_due(failed_at + DEBOUNCE).persist);
        assert!(c.persist.is_dirty(ConfigFile::App));
        assert_eq!(c.flush_due(failed_at + RETRY), Changes::NONE);
        assert!(!c.persist.is_dirty(ConfigFile::App));
        assert!(read(&dir, "app.json").contains("proxy_gfw"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_edit_during_debounce_loses_to_local_change() {
        let dir = temp_dir("local-wins");
        let (mut c, _) = controller(&dir);
        let t0 = Instant::now();
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, t0);
        std::fs::write(
            dir.join("app.json"),
            r#"{ "pac_listen": "127.0.0.1:0", "pac_auto_update_hours": 6 }"#,
        )
        .unwrap();
        let changes = c.on_files_changed(t0 + Duration::from_millis(100));
        assert!(
            !changes.overview,
            "the dirty local value must not be replaced"
        );
        assert!(changes.persist, "the pending save must stay armed");
        assert_eq!(c.gui.config.pac_rule_mode, PacRuleMode::ProxyGfw);
        let _ = c.flush_due(t0 + Duration::from_millis(500));
        let saved = read(&dir, "app.json");
        assert!(saved.contains("proxy_gfw"));
        assert!(saved.contains("\"pac_auto_update_hours\": 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_edit_of_clean_file_after_flush_reloads() {
        let dir = temp_dir("reload");
        let (mut c, _) = controller(&dir);
        let t0 = Instant::now();
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, t0);
        let _ = c.flush_due(t0 + DEBOUNCE);
        // Our own write is recognised and ignored.
        assert_eq!(c.on_files_changed(t0 + DEBOUNCE), Changes::NONE);
        std::fs::write(
            dir.join("app.json"),
            r#"{ "pac_listen": "127.0.0.1:0", "pac_rule_mode": "bypass_china" }"#,
        )
        .unwrap();
        let changes = c.on_files_changed(t0 + DEBOUNCE + Duration::from_millis(10));
        assert!(changes.overview);
        assert_eq!(c.gui.config.pac_rule_mode, PacRuleMode::BypassChina);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_between_load_and_watch_is_reloaded() {
        let dir = temp_dir("race");
        let (mut c, _) = controller(&dir);
        // Lands after loading but before the first watcher check.
        std::fs::write(
            dir.join("app.json"),
            r#"{ "pac_listen": "127.0.0.1:0", "pac_rule_mode": "proxy_gfw" }"#,
        )
        .unwrap();
        assert!(c.on_files_changed(Instant::now()).overview);
        assert_eq!(c.gui.config.pac_rule_mode, PacRuleMode::ProxyGfw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_edit_of_dirty_file_rearms_the_save() {
        let dir = temp_dir("dirty-rearm");
        let (mut c, _) = controller(&dir);
        let t0 = Instant::now();
        let _ = c.set_pac_rule(PacRuleMode::ProxyGfw, t0);
        std::fs::write(dir.join("app.json"), r#"{ "pac_listen": "127.0.0.1:0" }"#).unwrap();
        assert!(c.on_files_changed(t0).persist);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn info_notice_expires_but_errors_stay() {
        let dir = temp_dir("notice");
        let (mut c, _) = controller(&dir);
        let _ = c.copy_pac_url();
        let (notice, seq) = c.notice();
        assert_eq!(notice, Notice::LinkCopied);
        let _ = c.expire_notice(seq);
        assert_eq!(c.notice().0, Notice::None);
        let _ = c.finish_rules(Err(anyhow::anyhow!("offline")));
        let (_, seq) = c.notice();
        let _ = c.expire_notice(seq);
        assert!(c.notice().0.is_error());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
