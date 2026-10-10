//! Overview page: connection, system proxy mode, PAC rules.

use super::{Changes, ConfigFile, Controller, Notice};
use crate::config::{AppConfig, PacMode, PacRuleMode, ProxyProtocol, SystemProxyMode};
use crate::pac;
use crate::state::restart_pac_server;
use crate::util::{format_host_port, split_host_port};
use chrono::{DateTime, Local};
use std::path::PathBuf;
use std::time::Instant;

/// Everything the overview page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverviewSnapshot {
    pub running: bool,
    pub active_name: String,
    pub active_address: String,
    pub local_host: String,
    pub local_port: String,
    pub pac_url: String,
    pub proxy_mode: SystemProxyMode,
    pub pac_rule: PacRuleMode,
    /// Days since the rule download (-1: never) and the time or date to show.
    pub rules_age_days: i32,
    pub rules_time: String,
    pub rules_updating: bool,
}

/// A rule download to run on a worker thread.
pub struct RuleJob {
    dir: PathBuf,
    direct_url: String,
    proxy_url: String,
}

impl RuleJob {
    /// Blocking download; call off the UI thread.
    pub fn run(self) -> anyhow::Result<()> {
        pac::download_rules(&self.dir, &self.direct_url, &self.proxy_url).map(|_| ())
    }
}

/// URL the system proxy uses in PAC mode (mirrors `system_proxy.rs`).
fn effective_pac_url(config: &AppConfig) -> String {
    match (config.pac_mode, &config.online_pac_url) {
        (PacMode::Online, Some(url)) => url.clone(),
        _ => pac::pac_url(&config.pac_listen),
    }
}

/// Day offset and label for a rule timestamp: "HH:MM" for today and
/// yesterday, "YYYY-MM-DD" for older ones.
pub fn rules_stamp(updated: DateTime<Local>, now: DateTime<Local>) -> (i32, String) {
    let days = (now.date_naive() - updated.date_naive()).num_days().max(0);
    match days {
        0 | 1 => (days as i32, updated.format("%H:%M").to_string()),
        _ => (2, updated.format("%Y-%m-%d").to_string()),
    }
}

impl Controller {
    pub fn overview(&self) -> OverviewSnapshot {
        let config = &self.gui.config;
        let (host, port) = split_host_port(&config.mixed_listen);
        let (active_name, active_address) = match self.gui.selected_profile() {
            Some(profile) => {
                let protocol = match profile.protocol {
                    ProxyProtocol::Juicity => "juicity",
                    ProxyProtocol::Shadowsocks => "shadowsocks",
                };
                let address = if profile.server.is_empty() {
                    protocol.to_string()
                } else {
                    format!(
                        "{protocol} · {}",
                        format_host_port(&profile.server, profile.server_port)
                    )
                };
                (profile.display_name(), address)
            }
            None => (String::new(), String::new()),
        };
        let (rules_age_days, rules_time) =
            match pac::rules_updated_at(&self.gui.storage.paths().config_dir) {
                Some(time) => rules_stamp(time.into(), Local::now()),
                None => (-1, String::new()),
            };
        OverviewSnapshot {
            running: self.connected,
            active_name,
            active_address,
            local_host: host.to_string(),
            local_port: port.to_string(),
            pac_url: effective_pac_url(config),
            proxy_mode: config.system_proxy_mode,
            pac_rule: config.pac_rule_mode,
            rules_age_days,
            rules_time,
            rules_updating: self.rules_updating,
        }
    }

    /// Start the active node of the working list or stop the core. Only
    /// runtime state is written; unsaved node edits stay in memory.
    pub fn toggle_connection(&mut self, now: Instant) -> Changes {
        if self.connected {
            // Totals and history stay; the next counters start a new baseline.
            self.logs.traffic.reset();
            self.effects.stop_core(&mut self.gui.core_manager);
            self.connected = false;
            self.nodes.started_with = None;
            self.gui.runtime.was_running = false;
            return Changes::OVERVIEW | Changes::NODES | self.mark_dirty(ConfigFile::Runtime, now);
        }
        self.start_active(now)
    }

    /// Start (or restart, when connected) the core with the active node.
    /// A node with missing fields leaves a running core untouched.
    pub(super) fn start_active(&mut self, now: Instant) -> Changes {
        let Some(profile) = self.gui.selected_profile().cloned() else {
            return self.set_notice(Notice::NoNode);
        };
        let missing = crate::validate::missing_fields(&profile);
        if !missing.is_empty() {
            return self.set_notice(Notice::MissingFields(missing));
        }
        let config = self.gui.config.clone();
        // A new core restarts its byte counters.
        self.logs.traffic.reset();
        match self
            .effects
            .start_core(&mut self.gui.core_manager, &config, &profile)
        {
            Ok(()) => {
                self.connected = true;
                self.nodes.started_with = Some(profile);
                self.gui.runtime.was_running = true;
                let mut changes =
                    Changes::OVERVIEW | Changes::NODES | self.mark_dirty(ConfigFile::Runtime, now);
                if matches!(
                    self.notice,
                    Notice::StartFailed(_)
                        | Notice::CoreExited(_)
                        | Notice::NoNode
                        | Notice::MissingFields(_)
                ) {
                    changes |= self.dismiss_notice();
                }
                changes
            }
            Err(err) => {
                // A failed restart leaves no core running.
                let mut changes = Changes::OVERVIEW | Changes::NODES;
                if self.connected {
                    self.connected = false;
                    self.nodes.started_with = None;
                    self.gui.runtime.was_running = false;
                    changes |= self.mark_dirty(ConfigFile::Runtime, now);
                }
                changes | self.set_notice(Notice::StartFailed(format!("{err:#}")))
            }
        }
    }

    pub fn set_proxy_mode(&mut self, mode: SystemProxyMode, now: Instant) -> Changes {
        if self.gui.config.system_proxy_mode == mode {
            return Changes::NONE;
        }
        self.gui.config.system_proxy_mode = mode;
        let mut changes = Changes::OVERVIEW | self.mark_dirty(ConfigFile::App, now);
        if let Err(err) = self.effects.apply_system_proxy(&self.gui.config) {
            changes |= self.set_notice(Notice::ProxyFailed(format!("{err:#}")));
        }
        changes
    }

    pub fn set_pac_rule(&mut self, rule: PacRuleMode, now: Instant) -> Changes {
        if self.gui.config.pac_rule_mode == rule {
            return Changes::NONE;
        }
        self.gui.config.pac_rule_mode = rule;
        if let Err(err) = restart_pac_server(&mut self.gui, false) {
            tracing::warn!("PAC server update failed: {err:#}");
        }
        Changes::OVERVIEW | self.mark_dirty(ConfigFile::App, now)
    }

    pub(super) fn rules_overdue(&self) -> bool {
        let hours = self.gui.config.pac_auto_update_hours;
        hours > 0
            && pac::rules_age_hours(&self.gui.storage.paths().config_dir)
                .is_none_or(|age| age >= hours as u64)
    }

    pub(super) fn begin_rules(&mut self) -> Option<RuleJob> {
        if self.rules_updating {
            return None;
        }
        self.rules_updating = true;
        Some(RuleJob {
            dir: self.gui.storage.paths().config_dir.clone(),
            direct_url: self.gui.config.pac_direct_url.clone(),
            proxy_url: self.gui.config.pac_proxy_url.clone(),
        })
    }

    /// 更新規則: returns the download job unless one is already running.
    pub fn update_rules(&mut self) -> (Changes, Option<RuleJob>) {
        let job = self.begin_rules();
        let changes = if job.is_some() {
            Changes::OVERVIEW
        } else {
            Changes::NONE
        };
        (changes, job)
    }

    pub fn finish_rules(&mut self, result: anyhow::Result<()>) -> Changes {
        self.rules_updating = false;
        let notice = match result {
            Ok(()) => {
                if let Err(err) = restart_pac_server(&mut self.gui, false) {
                    tracing::warn!("PAC server update failed: {err:#}");
                }
                Notice::RulesUpdated
            }
            Err(err) => Notice::RulesFailed(format!("{err:#}")),
        };
        Changes::OVERVIEW | self.set_notice(notice)
    }

    pub fn copy_pac_url(&mut self) -> Changes {
        let url = effective_pac_url(&self.gui.config);
        match self.effects.copy_text(&url) {
            Ok(()) => self.set_notice(Notice::LinkCopied),
            Err(err) => {
                tracing::warn!("failed to copy the PAC URL: {err:#}");
                Changes::NONE
            }
        }
    }

    /// Poll timer: sample the traffic counters, then detect a core that
    /// exited on its own.
    pub fn poll_core(&mut self) -> Changes {
        self.sample_traffic();
        if !self.connected {
            return Changes::NONE;
        }
        match self.effects.poll_core(&mut self.gui.core_manager) {
            Some(reason) => {
                self.connected = false;
                self.nodes.started_with = None;
                Changes::OVERVIEW | Changes::NODES | self.set_notice(Notice::CoreExited(reason))
            }
            None => Changes::NONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn rules_stamp_uses_time_for_recent_days_and_date_otherwise() {
        let at = |d, h| Local.with_ymd_and_hms(2026, 10, d, h, 42, 0).unwrap();
        assert_eq!(rules_stamp(at(10, 9), at(10, 18)), (0, "09:42".into()));
        assert_eq!(rules_stamp(at(9, 23), at(10, 1)), (1, "23:42".into()));
        assert_eq!(rules_stamp(at(1, 9), at(10, 9)), (2, "2026-10-01".into()));
    }

    #[test]
    fn start_failure_and_core_exit_show_errors() {
        let dir = temp_dir("core");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        fake.0.borrow_mut().start_error = Some("bind failed".into());
        let _ = c.toggle_connection(now);
        assert!(!c.overview().running);
        assert_eq!(c.notice().0, Notice::StartFailed("bind failed".into()));
        fake.0.borrow_mut().start_error = None;
        let _ = c.toggle_connection(now);
        assert!(c.overview().running);
        assert_eq!(c.notice().0, Notice::None);
        fake.0.borrow_mut().exit_reason = Some("Juicity core exited".into());
        assert!(c.poll_core().overview);
        assert!(!c.overview().running);
        assert!(c.notice().0.is_error());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_reports_missing_fields_and_missing_node() {
        use crate::validate::RequiredField;
        let dir = temp_dir("missing");
        std::fs::write(
            dir.join("profiles.json"),
            r#"{ "profiles": [{ "server": "tokyo.example.com" }] }"#,
        )
        .unwrap();
        let (mut c, fake) = controller(&dir);
        let _ = c.toggle_connection(Instant::now());
        assert_eq!(
            c.notice().0,
            Notice::MissingFields(vec![RequiredField::Uuid, RequiredField::Password])
        );
        assert!(!fake.0.borrow().running);
        std::fs::write(dir.join("runtime.json"), r#"{ "selected_profile": 5 }"#).unwrap();
        let (mut c, _) = controller(&dir);
        let _ = c.toggle_connection(Instant::now());
        assert_eq!(c.notice().0, Notice::NoNode);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn proxy_mode_is_applied_and_restored_on_shutdown() {
        let dir = temp_dir("proxy");
        let (mut c, fake) = controller(&dir);
        let _ = c.set_proxy_mode(SystemProxyMode::Pac, Instant::now());
        c.shutdown();
        assert_eq!(
            fake.0.borrow().proxy_modes,
            [SystemProxyMode::Pac, SystemProxyMode::Disable]
        );
        // The saved mode stays PAC so the next start re-applies it.
        assert!(std::fs::read_to_string(dir.join("app.json"))
            .unwrap()
            .contains("\"system_proxy_mode\": \"pac\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
