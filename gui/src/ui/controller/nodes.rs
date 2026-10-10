//! Nodes page: list operations, the editor draft and the Advanced sheet.
//!
//! `NodesState::selected` is the node shown in the editor (UI only);
//! `runtime.selected_profile` is the working active node the core runs;
//! `saved.active` is the one on disk.

use super::{Changes, Controller, Notice};
use crate::config::{normalize_congestion_control, ProxyProfile, ProxyProtocol};
use crate::link;
use crate::state::{non_empty_text, GuiState};
use crate::util::format_host_port;
use crate::validate::{missing_fields, RequiredField};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftField {
    Name,
    Protocol,
    Server,
    Port,
    Uuid,
    Password,
    Method,
    Sni,
    AllowInsecure,
    PinnedCertchainSha256,
    CongestionControl,
    Plugin,
    PluginOptions,
    PluginArgs,
    Timeout,
    Group,
}

/// Why a draft field cannot be saved. The UI formats its own message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftError {
    /// A mandatory field (server, UUID, password) is empty.
    Required,
    /// Not a port in 1..=65535.
    Port,
    /// Not a whole number of seconds.
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListCommand {
    MoveUp,
    MoveDown,
    Duplicate,
    Delete,
}

/// Editor text for one node, as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftData {
    pub name: String,
    pub protocol: ProxyProtocol,
    pub server: String,
    pub port: String,
    pub uuid: String,
    pub password: String,
    pub method: String,
    pub sni: String,
    pub allow_insecure: bool,
    pub pinned_certchain_sha256: String,
    pub congestion_control: String,
    pub plugin: String,
    pub plugin_options: String,
    pub plugin_args: String,
    pub timeout: String,
    pub group: String,
}

fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

fn parse_timeout(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

impl DraftData {
    pub fn from_profile(profile: &ProxyProfile) -> Self {
        let text = |value: &Option<String>| value.clone().unwrap_or_default();
        Self {
            name: profile.name.clone(),
            protocol: profile.protocol,
            server: profile.server.clone(),
            port: profile.server_port.to_string(),
            uuid: profile.uuid.clone(),
            password: profile.password.clone(),
            method: profile.method.clone(),
            sni: text(&profile.sni),
            allow_insecure: profile.allow_insecure,
            pinned_certchain_sha256: text(&profile.pinned_certchain_sha256),
            // The core uses BBR when nothing is set.
            congestion_control: profile
                .congestion_control
                .clone()
                .unwrap_or_else(|| "bbr".to_string()),
            plugin: text(&profile.plugin),
            plugin_options: text(&profile.plugin_opts),
            plugin_args: text(&profile.plugin_args),
            timeout: profile.timeout.to_string(),
            group: text(&profile.group),
        }
    }

    fn set(&mut self, field: DraftField, value: &str) {
        let value = value.to_string();
        match field {
            DraftField::Name => self.name = value,
            DraftField::Protocol => {
                self.protocol = if value == "shadowsocks" {
                    ProxyProtocol::Shadowsocks
                } else {
                    ProxyProtocol::Juicity
                }
            }
            DraftField::Server => self.server = value,
            DraftField::Port => self.port = value,
            DraftField::Uuid => self.uuid = value,
            DraftField::Password => self.password = value,
            DraftField::Method => self.method = value,
            DraftField::Sni => self.sni = value,
            DraftField::AllowInsecure => self.allow_insecure = value == "true",
            DraftField::PinnedCertchainSha256 => self.pinned_certchain_sha256 = value,
            DraftField::CongestionControl => self.congestion_control = value,
            DraftField::Plugin => self.plugin = value,
            DraftField::PluginOptions => self.plugin_options = value,
            DraftField::PluginArgs => self.plugin_args = value,
            DraftField::Timeout => self.timeout = value,
            DraftField::Group => self.group = value,
        }
    }

    /// Copy the fields edited in the Advanced sheet.
    fn take_advanced(&mut self, sheet: &Self) {
        self.sni.clone_from(&sheet.sni);
        self.allow_insecure = sheet.allow_insecure;
        self.pinned_certchain_sha256
            .clone_from(&sheet.pinned_certchain_sha256);
        self.congestion_control
            .clone_from(&sheet.congestion_control);
        self.plugin.clone_from(&sheet.plugin);
        self.plugin_options.clone_from(&sheet.plugin_options);
        self.plugin_args.clone_from(&sheet.plugin_args);
        self.timeout.clone_from(&sheet.timeout);
        self.group.clone_from(&sheet.group);
    }

    /// Mandatory fields that are empty, per `validate::missing_fields`.
    fn missing(&self) -> Vec<DraftField> {
        let probe = ProxyProfile {
            protocol: self.protocol,
            server: self.server.clone(),
            uuid: self.uuid.clone(),
            password: self.password.clone(),
            method: self.method.clone(),
            ..Default::default()
        };
        missing_fields(&probe)
            .into_iter()
            .map(|field| match field {
                RequiredField::Server => DraftField::Server,
                RequiredField::Uuid => DraftField::Uuid,
                RequiredField::Password => DraftField::Password,
            })
            .collect()
    }

    pub fn errors(&self) -> Vec<(DraftField, DraftError)> {
        let mut errors: Vec<_> = self
            .missing()
            .into_iter()
            .map(|field| (field, DraftError::Required))
            .collect();
        if parse_port(&self.port).is_none() {
            errors.push((DraftField::Port, DraftError::Port));
        }
        if parse_timeout(&self.timeout).is_none() {
            errors.push((DraftField::Timeout, DraftError::Timeout));
        }
        errors
    }

    /// The text of a field that can hold invalid input.
    fn checked_text(&self, field: DraftField) -> &str {
        match field {
            DraftField::Server => &self.server,
            DraftField::Port => &self.port,
            DraftField::Uuid => &self.uuid,
            DraftField::Password => &self.password,
            DraftField::Timeout => &self.timeout,
            // `errors` reports no other field.
            _ => "",
        }
    }

    /// Invalid text that `apply_to` kept out of `profile`.
    fn unapplied(&self, profile: &ProxyProfile) -> bool {
        let stored = Self::from_profile(profile);
        self.errors()
            .iter()
            .any(|(field, _)| self.checked_text(*field) != stored.checked_text(*field))
    }

    /// Write every changed, valid field into `profile`; invalid or emptied
    /// mandatory fields keep the profile's last valid value.
    fn apply_to(&self, profile: &mut ProxyProfile) {
        let current = Self::from_profile(profile);
        let missing = self.missing();
        if self.name != current.name {
            profile.name = self.name.trim().to_string();
        }
        profile.protocol = self.protocol;
        if self.server != current.server && !missing.contains(&DraftField::Server) {
            profile.server = self.server.trim().to_string();
        }
        if let Some(port) = parse_port(&self.port) {
            profile.server_port = port;
        }
        if self.uuid != current.uuid && !missing.contains(&DraftField::Uuid) {
            profile.uuid = self.uuid.trim().to_string();
        }
        if !missing.contains(&DraftField::Password) {
            profile.password.clone_from(&self.password);
        }
        profile.method.clone_from(&self.method);
        if self.sni != current.sni {
            profile.sni = non_empty_text(&self.sni);
        }
        profile.allow_insecure = self.allow_insecure;
        if self.pinned_certchain_sha256 != current.pinned_certchain_sha256 {
            profile.pinned_certchain_sha256 = non_empty_text(&self.pinned_certchain_sha256);
        }
        if self.congestion_control != current.congestion_control {
            profile.congestion_control = normalize_congestion_control(&self.congestion_control);
        }
        if self.plugin != current.plugin {
            profile.plugin = non_empty_text(&self.plugin);
        }
        if self.plugin_options != current.plugin_options {
            profile.plugin_opts = non_empty_text(&self.plugin_options);
        }
        if self.plugin_args != current.plugin_args {
            profile.plugin_args = non_empty_text(&self.plugin_args);
        }
        if let Some(timeout) = parse_timeout(&self.timeout) {
            profile.timeout = timeout;
        }
        if self.group != current.group {
            profile.group = non_empty_text(&self.group);
        }
    }
}

/// One row of the node list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowData {
    pub name: String,
    pub address: String,
    pub protocol: ProxyProtocol,
    pub in_use: bool,
    pub group: String,
}

/// Everything the nodes page shows except the editor's text fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodesSnapshot {
    pub rows: Vec<RowData>,
    pub selected: usize,
    pub draft: DraftData,
    pub errors: Vec<(DraftField, DraftError)>,
    pub reconnect_required: bool,
    /// The Advanced sheet draft while the sheet is open.
    pub advanced: Option<DraftData>,
    /// The working list differs from the saved one.
    pub dirty: bool,
    /// The unsaved-changes prompt is open.
    pub prompt: bool,
}

pub(super) struct NodesState {
    pub(super) selected: usize,
    pub(super) draft: DraftData,
    pub(super) errors: Vec<(DraftField, DraftError)>,
    pub(super) advanced: Option<DraftData>,
    pub(super) advanced_errors: Vec<(DraftField, DraftError)>,
    /// The profile the running core was started with.
    pub(super) started_with: Option<ProxyProfile>,
}

impl NodesState {
    pub(super) fn new(gui: &GuiState) -> Self {
        let last = gui.profiles.profiles.len().saturating_sub(1);
        let selected = gui.runtime.selected_profile.min(last);
        Self {
            selected,
            draft: gui
                .profiles
                .profiles
                .get(selected)
                .map(DraftData::from_profile)
                .unwrap_or_default(),
            errors: Vec::new(),
            advanced: None,
            advanced_errors: Vec::new(),
            started_with: None,
        }
    }
}

impl Controller {
    pub fn nodes(&self) -> NodesSnapshot {
        let active = self.gui.runtime.selected_profile;
        let profiles = &self.gui.profiles.profiles;
        let rows = profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| RowData {
                name: profile.display_name(),
                address: if profile.server.is_empty() {
                    String::new()
                } else {
                    format_host_port(&profile.server, profile.server_port)
                },
                protocol: profile.protocol,
                in_use: index == active,
                group: profile.group.clone().unwrap_or_default(),
            })
            .collect();
        let state = &self.nodes;
        let mut errors = state.errors.clone();
        errors.extend(state.advanced_errors.iter().copied());
        NodesSnapshot {
            rows,
            selected: state.selected,
            draft: state.draft.clone(),
            errors,
            // Unsaved edits are not applied; compare the saved version of
            // the active node (its index may not be saved yet).
            reconnect_required: self.connected
                && state.selected == active
                && state.started_with.as_ref() != self.saved.profiles.profiles.get(active),
            advanced: state.advanced.clone(),
            dirty: self.nodes_dirty(),
            prompt: self.prompt_open(),
        }
    }

    pub fn node_count(&self) -> usize {
        self.gui.profiles.profiles.len()
    }

    /// Reload the editor from the selected profile, dropping invalid text.
    pub(super) fn load_selected(&mut self) -> Changes {
        let last = self.gui.profiles.profiles.len().saturating_sub(1);
        let state = &mut self.nodes;
        state.selected = state.selected.min(last);
        state.draft = self
            .gui
            .profiles
            .profiles
            .get(state.selected)
            .map(DraftData::from_profile)
            .unwrap_or_default();
        state.errors.clear();
        Changes::EDITOR
    }

    /// The editor shows invalid text that is not in the working list.
    pub(super) fn draft_unapplied(&self) -> bool {
        self.gui
            .profiles
            .profiles
            .get(self.nodes.selected)
            .is_some_and(|profile| self.nodes.draft.unapplied(profile))
    }

    pub fn select_node(&mut self, index: usize) -> Changes {
        if index == self.nodes.selected || index >= self.gui.profiles.profiles.len() {
            return Changes::NONE;
        }
        // Deliberate simplification: invalid text of the node left behind is dropped.
        self.nodes.selected = index;
        self.load_selected()
    }

    /// Add node: append a default node named `name` and select it.
    pub fn add_node(&mut self, name: String) -> Changes {
        self.gui.profiles.profiles.push(ProxyProfile {
            name,
            ..Default::default()
        });
        self.nodes.selected = self.gui.profiles.profiles.len() - 1;
        self.load_selected()
    }

    /// Row context menu and footer actions on the node at `index`. Deleting
    /// the last node is refused.
    pub fn node_command(&mut self, index: usize, command: ListCommand, now: Instant) -> Changes {
        let len = self.gui.profiles.profiles.len();
        if index >= len {
            return Changes::NONE;
        }
        let active = self.gui.runtime.selected_profile;
        let selected = self.nodes.selected;
        let deletes_active = command == ListCommand::Delete && index == active;
        // Maps an old index to the same node's new index.
        let remap: Box<dyn Fn(usize) -> usize> = match command {
            ListCommand::MoveUp | ListCommand::MoveDown => {
                let other = match command {
                    ListCommand::MoveUp if index > 0 => index - 1,
                    ListCommand::MoveDown if index + 1 < len => index + 1,
                    _ => return Changes::NONE,
                };
                self.gui.profiles.profiles.swap(index, other);
                Box::new(move |i| match i {
                    i if i == index => other,
                    i if i == other => index,
                    i => i,
                })
            }
            ListCommand::Duplicate => {
                let copy = self.gui.profiles.profiles[index].clone();
                self.gui.profiles.profiles.insert(index + 1, copy);
                Box::new(move |i| if i > index { i + 1 } else { i })
            }
            ListCommand::Delete => {
                if len == 1 {
                    return Changes::NONE;
                }
                self.gui.profiles.profiles.remove(index);
                Box::new(move |i| if i > index { i - 1 } else { i.min(len - 2) })
            }
        };
        let mut changes = Changes::OVERVIEW | Changes::NODES;
        // The saved active index keeps pointing into the saved list.
        self.gui.runtime.selected_profile = remap(active);
        match command {
            ListCommand::Duplicate => {
                self.nodes.selected = index + 1;
                changes |= self.load_selected();
            }
            ListCommand::Delete if selected == index => {
                self.nodes.selected = remap(selected);
                changes |= self.load_selected();
            }
            _ => {
                self.nodes.selected = remap(selected);
                changes |= Changes::NODES;
            }
        }
        // The core must not keep running a node that no longer exists.
        // An incomplete replacement stops the core; start_active then explains why.
        if deletes_active && self.connected {
            let incomplete = self
                .gui
                .selected_profile()
                .is_none_or(|p| !crate::validate::missing_fields(p).is_empty());
            if incomplete {
                changes |= self.toggle_connection(now);
            }
            changes |= self.start_active(now);
        }
        changes
    }

    /// An editor field changed: validate the draft and copy its valid
    /// fields into the working list.
    pub fn edit_node(&mut self, field: DraftField, value: &str) -> Changes {
        self.nodes.draft.set(field, value);
        let changes = self.commit_draft();
        // Menu choices change which fields the editor shows.
        if matches!(field, DraftField::Protocol | DraftField::Method) {
            changes | Changes::EDITOR
        } else {
            changes
        }
    }

    fn commit_draft(&mut self) -> Changes {
        self.nodes.errors = self.nodes.draft.errors();
        let index = self.nodes.selected;
        let Some(profile) = self.gui.profiles.profiles.get_mut(index) else {
            return Changes::NODES;
        };
        let before = profile.clone();
        self.nodes.draft.apply_to(profile);
        if *profile == before {
            return Changes::NODES;
        }
        if index == self.gui.runtime.selected_profile {
            Changes::NODES | Changes::OVERVIEW
        } else {
            Changes::NODES
        }
    }

    /// Set as active: make the selected node active; a running core switches to
    /// it immediately (as the tray selection does).
    pub fn set_active_node(&mut self, now: Instant) -> Changes {
        let index = self.nodes.selected;
        let Some(target) = self.gui.profiles.profiles.get(index) else {
            return Changes::NONE;
        };
        // An incomplete node never becomes active; the old one stays.
        let missing = missing_fields(target);
        if !missing.is_empty() {
            return self.set_notice(Notice::MissingFields(missing));
        }
        self.set_working_active(index);
        let mut changes = Changes::OVERVIEW | Changes::NODES;
        if self.connected {
            changes |= self.start_active(now);
        }
        changes
    }

    /// Import link: add every link on the clipboard and select the first one.
    pub fn import_links(&mut self) -> Changes {
        match self.effects.paste_text() {
            Ok(text) => self.import_text(&text),
            Err(err) => self.set_notice(Notice::ImportFailed(format!("{err:#}"))),
        }
    }

    pub fn import_text(&mut self, text: &str) -> Changes {
        let first = self.gui.profiles.profiles.len();
        let mut skipped = 0;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            // Links carry secrets: neither they nor parse errors are logged.
            match link::import_share_link(line) {
                Ok(imported) => {
                    let mut profile = ProxyProfile::default();
                    imported.apply_to(&mut profile);
                    self.gui.profiles.profiles.push(profile);
                }
                Err(_) => skipped += 1,
            }
        }
        let added = self.gui.profiles.profiles.len() - first;
        if skipped > 0 {
            tracing::warn!(
                "skipped {skipped} clipboard lines that are not juicity:// or ss:// links"
            );
        }
        if added == 0 {
            let reason = if skipped == 0 {
                "the clipboard holds no link"
            } else {
                "no line is a valid juicity:// or ss:// link"
            };
            return self.set_notice(Notice::ImportFailed(reason.to_string()));
        }
        let notice = if skipped == 0 {
            Notice::Imported
        } else {
            Notice::ImportPartial(added, skipped)
        };
        self.nodes.selected = first;
        self.load_selected() | self.set_notice(notice)
    }

    /// Export link: copy the selected node's share link.
    pub fn export_link(&mut self) -> Changes {
        // The stored profile would not match what the editor shows.
        if !self.nodes.errors.is_empty() {
            return self.set_notice(Notice::ExportInvalid);
        }
        let Some(profile) = self.gui.profiles.profiles.get(self.nodes.selected) else {
            return Changes::NONE;
        };
        let copied =
            link::export_share_link(profile).and_then(|link| self.effects.copy_text(&link));
        match copied {
            Ok(()) => self.set_notice(Notice::LinkCopied),
            Err(err) => self.set_notice(Notice::ExportFailed(format!("{err:#}"))),
        }
    }

    pub fn open_advanced(&mut self) -> Changes {
        self.nodes.advanced = Some(self.nodes.draft.clone());
        self.nodes.advanced_errors.clear();
        Changes::NODES
    }

    /// A sheet field changed; nothing is saved until Done.
    pub fn edit_advanced(&mut self, field: DraftField, value: &str) -> Changes {
        let Some(sheet) = &mut self.nodes.advanced else {
            return Changes::NONE;
        };
        sheet.set(field, value);
        self.nodes.advanced_errors = sheet
            .errors()
            .into_iter()
            .filter(|(field, _)| *field == DraftField::Timeout)
            .collect();
        Changes::NODES
    }

    /// Done: keep the sheet open while it has errors, else copy it into
    /// the working list.
    pub fn commit_advanced(&mut self) -> Changes {
        if !self.nodes.advanced_errors.is_empty() {
            return Changes::NONE;
        }
        let Some(sheet) = self.nodes.advanced.take() else {
            return Changes::NONE;
        };
        self.nodes.draft.take_advanced(&sheet);
        self.commit_draft() | Changes::EDITOR
    }

    /// Cancel, Esc or a click on the scrim: drop the sheet draft.
    pub fn cancel_advanced(&mut self) -> Changes {
        self.nodes.advanced = None;
        self.nodes.advanced_errors.clear();
        Changes::NODES
    }

    /// The profile shown in the editor, captured before a reload.
    pub(super) fn edited_profile(&self) -> Option<ProxyProfile> {
        self.gui.profiles.profiles.get(self.nodes.selected).cloned()
    }

    /// After a reload from disk, reload the editor only when the node it
    /// shows actually changed, so typed text survives unrelated reloads.
    pub(super) fn after_reload(&mut self, edited: Option<ProxyProfile>) -> Changes {
        let last = self.gui.profiles.profiles.len().saturating_sub(1);
        self.nodes.selected = self.nodes.selected.min(last);
        if self.edited_profile() != edited {
            self.nodes.advanced = None;
            self.nodes.advanced_errors.clear();
            self.load_selected()
        } else {
            Changes::NODES
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    const TWO_NODES: &str = r#"{ "profiles": [
        { "name": "Tokyo 01", "server": "tokyo.example.com", "server_port": 443, "uuid": "u", "password": "p" },
        { "name": "Osaka 01", "server": "osaka.example.com", "server_port": 443, "uuid": "u", "password": "p" }
    ] }"#;

    fn names(c: &Controller) -> Vec<String> {
        c.nodes().rows.into_iter().map(|row| row.name).collect()
    }

    fn two_nodes(name: &str) -> (std::path::PathBuf, Controller, FakeEffects) {
        let dir = temp_dir(name);
        std::fs::write(dir.join("profiles.json"), TWO_NODES).unwrap();
        let (c, fake) = controller(&dir);
        (dir, c, fake)
    }

    #[test]
    fn list_operations_keep_selection_and_active_node() {
        let (dir, mut c, _) = two_nodes("nodes-list");
        let now = Instant::now();
        // Active node 0 (Tokyo); select Osaka and move it up.
        assert!(c.select_node(1).editor);
        let _ = c.node_command(1, ListCommand::MoveUp, now);
        assert_eq!(names(&c), ["Osaka 01", "Tokyo 01"]);
        assert_eq!(c.nodes().selected, 0);
        assert!(
            c.nodes().rows[1].in_use,
            "the active node moved with its row"
        );
        assert_eq!(c.gui.runtime.selected_profile, 1);
        // Moving past the ends does nothing.
        assert_eq!(c.node_command(0, ListCommand::MoveUp, now), Changes::NONE);
        assert_eq!(c.node_command(1, ListCommand::MoveDown, now), Changes::NONE);
        // Duplicate selects the copy and keeps the active node.
        assert!(c.node_command(0, ListCommand::Duplicate, now).editor);
        assert_eq!(names(&c), ["Osaka 01", "Osaka 01", "Tokyo 01"]);
        assert_eq!(c.nodes().selected, 1);
        assert_eq!(c.gui.runtime.selected_profile, 2);
        // Deleting a row before the active node shifts the active index.
        let _ = c.node_command(0, ListCommand::Delete, now);
        assert_eq!(c.gui.runtime.selected_profile, 1);
        assert_eq!(c.nodes().selected, 0);
        // Deleting the active last row keeps a valid active index.
        let _ = c.node_command(1, ListCommand::Delete, now);
        assert_eq!(names(&c), ["Osaka 01"]);
        assert_eq!(c.gui.runtime.selected_profile, 0);
        assert_eq!(c.nodes().selected, 0);
        // The last node cannot be deleted.
        assert_eq!(c.node_command(0, ListCommand::Delete, now), Changes::NONE);
        assert_eq!(c.node_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_port_is_rejected_and_not_saved() {
        let dir = temp_dir("nodes-port");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let before = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        let _ = c.edit_node(DraftField::Port, "70000");
        assert_eq!(
            c.gui.profiles, c.saved.profiles,
            "invalid text is not applied"
        );
        assert!(c.nodes().dirty, "but it is unsaved");
        assert_eq!(c.nodes().errors, [(DraftField::Port, DraftError::Port)]);
        assert_eq!(c.gui.profiles.profiles[0].server_port, 443);
        // A valid edit of another field is applied; the port keeps 443.
        let _ = c.edit_node(DraftField::Name, "東京 01");
        assert!(c.nodes().dirty);
        // Save refuses while the editor shows the invalid port.
        let _ = c.save_nodes();
        assert_eq!(c.notice().0, Notice::SaveInvalid);
        let read = || std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        assert_eq!(read(), before);
        // Switching nodes discards the invalid text.
        let _ = c.add_node("Node 2".into());
        let _ = c.select_node(0);
        assert_eq!(c.nodes().draft.port, "443");
        assert!(c.nodes().errors.is_empty());
        let _ = c.node_command(1, ListCommand::Delete, now);
        let _ = c.save_nodes();
        let saved = read();
        assert!(saved.contains("東京 01") && saved.contains("\"server_port\": 443"));
        let _ = c.edit_node(DraftField::Port, "0");
        assert_eq!(
            c.gui.profiles.profiles[0].server_port, 443,
            "port 0 is rejected"
        );
        let _ = c.edit_node(DraftField::Port, "8443");
        assert_eq!(c.gui.profiles.profiles[0].server_port, 8443);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advanced_sheet_commits_only_on_done() {
        let dir = temp_dir("nodes-sheet");
        let (mut c, _) = controller(&dir);
        let _ = c.open_advanced();
        let _ = c.edit_advanced(DraftField::Sni, "front.example.com");
        let _ = c.edit_advanced(DraftField::CongestionControl, "cubic");
        assert_eq!(c.gui.profiles.profiles[0].sni, None);
        let _ = c.cancel_advanced();
        assert_eq!(c.nodes().advanced, None);
        assert_eq!(c.nodes().draft.sni, "");
        let _ = c.open_advanced();
        let _ = c.edit_advanced(DraftField::Timeout, "soon");
        assert_eq!(c.commit_advanced(), Changes::NONE);
        assert_eq!(
            c.nodes().errors,
            [(DraftField::Timeout, DraftError::Timeout)]
        );
        let _ = c.edit_advanced(DraftField::Timeout, "10");
        let _ = c.edit_advanced(DraftField::PinnedCertchainSha256, "aGFzaA");
        let _ = c.edit_advanced(DraftField::CongestionControl, "cubic");
        assert!(c.commit_advanced().nodes);
        assert!(c.nodes().dirty);
        let profile = &c.gui.profiles.profiles[0];
        assert_eq!(profile.timeout, 10);
        assert_eq!(profile.pinned_certchain_sha256.as_deref(), Some("aGFzaA"));
        assert_eq!(profile.congestion_control.as_deref(), Some("cubic"));
        assert_eq!(c.nodes().advanced, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_active_switches_running_core_and_edits_need_reconnect() {
        let (dir, mut c, fake) = two_nodes("nodes-active");
        let now = Instant::now();
        let _ = c.toggle_connection(now);
        let _ = c.select_node(1);
        assert!(!c.nodes().reconnect_required);
        let _ = c.set_active_node(now);
        assert_eq!(fake.0.borrow().started, ["Tokyo 01", "Osaka 01"]);
        assert!(c.nodes().rows[1].in_use);
        assert_eq!(c.overview().active_name, "Osaka 01");
        // Editing the active node never restarts the core; once saved,
        // the editor asks for a reconnect.
        let _ = c.edit_node(DraftField::Server, "kix.example.com");
        assert!(!c.nodes().reconnect_required);
        let _ = c.save_nodes();
        assert!(c.nodes().reconnect_required);
        assert_eq!(fake.0.borrow().started.len(), 2);
        let _ = c.select_node(0);
        assert!(
            !c.nodes().reconnect_required,
            "only shown for the active node"
        );
        // Disconnected: Set as active only changes the active node.
        let _ = c.toggle_connection(now);
        let _ = c.set_active_node(now);
        assert_eq!(fake.0.borrow().started.len(), 2);
        assert_eq!(c.gui.runtime.selected_profile, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_adds_and_selects_links_and_export_copies() {
        let dir = temp_dir("nodes-import");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().paste = "juicity://u:p@a.example.com:443?congestion_control=cubic#A\n\
             not a link\nss://YWVzLTI1Ni1nY206cGFzcw@127.0.0.1:8388#B\n"
            .into();
        assert!(c.import_links().editor);
        assert_eq!(c.notice().0, Notice::ImportPartial(2, 1));
        assert_eq!(names(&c), ["Tokyo 01", "A", "B"]);
        assert_eq!(c.nodes().selected, 1);
        assert_eq!(c.nodes().draft.congestion_control, "cubic");
        let _ = c.export_link();
        assert_eq!(c.notice().0, Notice::LinkCopied);
        let copied = fake.0.borrow().copied.clone();
        assert!(copied[0].starts_with("juicity://u:p@a.example.com:443?"));
        assert!(copied[0].contains("congestion_control=cubic"));
        fake.0.borrow_mut().paste = "juicity://secret-uuid@broken".into();
        let _ = c.import_links();
        let Notice::ImportFailed(reason) = c.notice().0 else {
            panic!("import must fail");
        };
        assert!(!reason.contains("secret"), "{reason}");
        assert_eq!(c.node_count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_of_other_files_keeps_typed_text() {
        let dir = temp_dir("nodes-reload");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let _ = c.edit_node(DraftField::Port, "70000");
        std::fs::write(dir.join("runtime.json"), r#"{ "close_to_tray": false }"#).unwrap();
        let changes = c.on_files_changed(now);
        assert!(changes.overview && !changes.editor);
        assert_eq!(c.nodes().draft.port, "70000");
        // An external edit of the edited node reloads the editor.
        std::fs::write(
            dir.join("profiles.json"),
            r#"{ "profiles": [{ "name": "Tokyo 02", "server": "t.example.com" }] }"#,
        )
        .unwrap();
        assert!(c.on_files_changed(now).editor);
        assert_eq!(c.nodes().draft.name, "Tokyo 02");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_active_node_while_connected_restarts_the_core() {
        let (dir, mut c, fake) = two_nodes("nodes-delete-active");
        let now = Instant::now();
        let _ = c.toggle_connection(now);
        let _ = c.node_command(0, ListCommand::Delete, now);
        assert_eq!(fake.0.borrow().started, ["Tokyo 01", "Osaka 01"]);
        assert!(fake.0.borrow().running);
        assert_eq!(c.overview().active_name, "Osaka 01");
        assert!(c.overview().running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_active_node_stops_the_core_when_the_replacement_is_incomplete() {
        let dir = temp_dir("nodes-delete-active-incomplete");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        let _ = c.toggle_connection(now);
        let _ = c.add_node("Node 2".into());
        let _ = c.node_command(0, ListCommand::Delete, now);
        assert_eq!(fake.0.borrow().started, ["Tokyo 01"]);
        assert!(!fake.0.borrow().running);
        assert!(!c.overview().running);
        assert!(matches!(c.notice().0, Notice::MissingFields(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_node_is_not_made_active() {
        let dir = temp_dir("nodes-active-incomplete");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        let _ = c.toggle_connection(now);
        let _ = c.add_node("Node 2".into());
        let _ = c.set_active_node(now);
        assert_eq!(c.gui.runtime.selected_profile, 0);
        assert_eq!(fake.0.borrow().started, ["Tokyo 01"]);
        assert!(matches!(c.notice().0, Notice::MissingFields(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emptied_required_fields_keep_the_stored_value() {
        let dir = temp_dir("nodes-required");
        let (mut c, fake) = controller(&dir);
        let _ = c.edit_node(DraftField::Server, "");
        assert_eq!(c.gui.profiles, c.saved.profiles);
        assert_eq!(
            c.nodes().errors,
            [(DraftField::Server, DraftError::Required)]
        );
        assert_eq!(c.gui.profiles.profiles[0].server, "tokyo.example.com");
        let _ = c.edit_node(DraftField::Password, "");
        assert_eq!(c.gui.profiles.profiles[0].password, "p");
        // Export refuses while the editor shows errors.
        let _ = c.export_link();
        assert_eq!(c.notice().0, Notice::ExportInvalid);
        assert!(fake.0.borrow().copied.is_empty());
        // A new node with empty fields cannot be saved.
        let before = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        let _ = c.add_node("Node 2".into());
        let _ = c.save_nodes();
        assert!(c.notice().0.is_error());
        let saved = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        assert_eq!(saved, before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn congestion_control_is_normalized_in_the_editor() {
        let dir = temp_dir("nodes-cc");
        let (mut c, _) = controller(&dir);
        for (input, stored) in [("NewReno", Some("new_reno")), ("vegas", None)] {
            let _ = c.open_advanced();
            let _ = c.edit_advanced(DraftField::CongestionControl, input);
            let _ = c.commit_advanced();
            assert_eq!(
                c.gui.profiles.profiles[0].congestion_control.as_deref(),
                stored
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
