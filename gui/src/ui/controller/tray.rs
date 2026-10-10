//! Tray menu state and tray actions.

use super::{Changes, Controller, Notice, RuleJob};
use crate::desktop::tray::{TrayEvent, TrayModel};
use crate::validate::missing_fields;
use std::time::Instant;

impl Controller {
    /// What the tray shows; labels are added by the bind layer.
    pub fn tray_model(&self) -> TrayModel {
        let profiles = &self.gui.profiles.profiles;
        let selected = self.gui.runtime.selected_profile;
        let active = (selected < profiles.len()).then_some(selected);
        TrayModel {
            connected: self.connected,
            active_name: active
                .map(|i| profiles[i].display_name())
                .unwrap_or_default(),
            proxy_mode: self.gui.config.system_proxy_mode,
            pac_rule: self.gui.config.pac_rule_mode,
            nodes: profiles.iter().map(|p| p.display_name()).collect(),
            active,
        }
    }

    pub fn close_to_tray(&self) -> bool {
        self.gui.runtime.close_to_tray
    }

    pub fn hide_on_start(&self) -> bool {
        self.gui.runtime.hide_window_on_startup
    }

    /// Handle a tray menu action. Window actions (show, hide, pages, quit)
    /// belong to the bind layer and change nothing here.
    pub fn on_tray(&mut self, event: TrayEvent, now: Instant) -> (Changes, Option<RuleJob>) {
        let changes = match event {
            TrayEvent::ToggleConnection => self.toggle_connection(now),
            TrayEvent::SetProxyMode(mode) => self.set_proxy_mode(mode, now),
            TrayEvent::SetPacRule(rule) => self.set_pac_rule(rule, now),
            TrayEvent::UpdateRules => return self.update_rules(),
            TrayEvent::SelectNode(index) => self.activate_node(index, now),
            TrayEvent::ImportClipboard => self.import_links(),
            TrayEvent::ToggleWindow
            | TrayEvent::Open
            | TrayEvent::ShowNodes
            | TrayEvent::ShowLogs
            | TrayEvent::ShowSettings
            | TrayEvent::ShowAbout
            | TrayEvent::Quit => Changes::NONE,
        };
        (changes, None)
    }

    /// Make node `index` active without moving the editor selection; a
    /// running core switches to it at once.
    fn activate_node(&mut self, index: usize, now: Instant) -> Changes {
        let Some(target) = self.gui.profiles.profiles.get(index) else {
            return Changes::NONE;
        };
        let missing = missing_fields(target);
        if !missing.is_empty() {
            return self.set_notice(Notice::MissingFields(missing));
        }
        if self.gui.runtime.selected_profile == index {
            return Changes::NONE;
        }
        let mut changes = Changes::OVERVIEW | Changes::NODES | self.set_working_active(index, now);
        if self.connected {
            changes |= self.start_active(now);
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::config::{PacRuleMode, SystemProxyMode};

    const OSAKA: &str = "juicity://u:p@osaka.example.com:443?congestion_control=bbr#Osaka";

    #[test]
    fn model_lists_nodes_and_marks_the_active_one() {
        let dir = temp_dir("tray-model");
        let (mut c, fake) = controller(&dir);
        let model = c.tray_model();
        assert!(!model.connected);
        assert_eq!(model.nodes, ["Tokyo 01"]);
        assert_eq!(model.active, Some(0));
        assert_eq!(model.active_name, "Tokyo 01");
        assert_eq!(model.proxy_mode, SystemProxyMode::Disable);
        assert_eq!(model.pac_rule, c.gui.config.pac_rule_mode);

        fake.0.borrow_mut().paste = OSAKA.into();
        let now = Instant::now();
        let _ = c.on_tray(TrayEvent::ImportClipboard, now);
        let _ = c.on_tray(TrayEvent::ToggleConnection, now);
        let _ = c.on_tray(TrayEvent::SetProxyMode(SystemProxyMode::Pac), now);
        let model = c.tray_model();
        assert!(model.connected);
        assert_eq!(model.nodes, ["Tokyo 01", "Osaka"]);
        assert_eq!(model.active, Some(0));
        assert_eq!(model.proxy_mode, SystemProxyMode::Pac);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_selection_has_no_active_node() {
        let dir = temp_dir("tray-none");
        let (mut c, _) = controller(&dir);
        c.gui.runtime.selected_profile = 7;
        let model = c.tray_model();
        assert_eq!(model.nodes.len(), 1);
        assert_eq!(model.active, None);
        assert_eq!(model.active_name, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tray_actions_reach_the_controller() {
        let dir = temp_dir("tray-actions");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        let (changes, _) = c.on_tray(TrayEvent::ToggleConnection, now);
        assert!(changes.overview);
        assert!(fake.0.borrow().running);
        let _ = c.on_tray(TrayEvent::SetProxyMode(SystemProxyMode::Global), now);
        assert_eq!(fake.0.borrow().proxy_modes, [SystemProxyMode::Global]);
        let _ = c.on_tray(TrayEvent::SetPacRule(PacRuleMode::ProxyGfw), now);
        assert_eq!(c.gui.config.pac_rule_mode, PacRuleMode::ProxyGfw);
        let _ = c.on_tray(TrayEvent::ToggleConnection, now);
        assert!(!fake.0.borrow().running);
        for event in [
            TrayEvent::Open,
            TrayEvent::ShowLogs,
            TrayEvent::ShowAbout,
            TrayEvent::Quit,
        ] {
            assert_eq!(c.on_tray(event, now).0, Changes::NONE);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selecting_a_node_while_connected_restarts_the_core() {
        let dir = temp_dir("tray-select");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        fake.0.borrow_mut().paste = OSAKA.into();
        let _ = c.on_tray(TrayEvent::ImportClipboard, now);
        assert_eq!(c.notice().0, Notice::Imported);
        let editor = c.nodes().selected;
        let _ = c.on_tray(TrayEvent::SelectNode(0), now);
        let _ = c.on_tray(TrayEvent::ToggleConnection, now);
        let (changes, _) = c.on_tray(TrayEvent::SelectNode(1), now);
        assert!(changes.overview);
        // Osaka is not saved yet, so its index is not persisted.
        c.flush_all(now).unwrap();
        let runtime = std::fs::read_to_string(dir.join("runtime.json")).unwrap();
        assert!(runtime.contains("\"selected_profile\": 0"), "{runtime}");
        assert_eq!(fake.0.borrow().started, ["Tokyo 01", "Osaka"]);
        assert!(fake.0.borrow().running);
        assert_eq!(c.tray_model().active, Some(1));
        assert_eq!(c.nodes().selected, editor, "the editor selection stays");
        // The active node again, or one out of range: nothing happens.
        assert_eq!(c.on_tray(TrayEvent::SelectNode(1), now).0, Changes::NONE);
        assert_eq!(c.on_tray(TrayEvent::SelectNode(9), now).0, Changes::NONE);
        assert_eq!(fake.0.borrow().started.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selecting_a_node_while_stopped_does_not_start() {
        let dir = temp_dir("tray-select-stopped");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().paste = OSAKA.into();
        let now = Instant::now();
        let _ = c.on_tray(TrayEvent::ImportClipboard, now);
        let _ = c.on_tray(TrayEvent::SelectNode(1), now);
        assert_eq!(c.tray_model().active, Some(1));
        assert!(fake.0.borrow().started.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clipboard_import_failure_is_reported() {
        let dir = temp_dir("tray-import");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().paste = "not a link".into();
        let (changes, _) = c.on_tray(TrayEvent::ImportClipboard, Instant::now());
        assert!(changes.notice);
        assert!(matches!(c.notice().0, Notice::ImportFailed(_)));
        assert_eq!(c.tray_model().nodes.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
