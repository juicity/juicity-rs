//! Explicit save for nodes: Save, Revert and the unsaved-changes prompt
//! shown before the window closes or the app quits.
//!
//! Every node edit changes only the working list (`gui.profiles`); disk
//! holds `saved` until Save writes the working list synchronously.

use super::{Changes, ConfigFile, Controller, Notice};
use crate::validate::missing_fields;
use std::time::Instant;

/// What a close request does once it is allowed to proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// Hide the window to the tray.
    Hide,
    /// Quit the application.
    Quit,
}

/// The answer to the unsaved-changes prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Save,
    Discard,
    Cancel,
}

impl Controller {
    /// The working node list differs from the saved one.
    pub fn nodes_dirty(&self) -> bool {
        self.gui.profiles != self.saved.profiles
    }

    /// The unsaved-changes prompt is waiting for an answer.
    pub fn prompt_open(&self) -> bool {
        self.leave.is_some()
    }

    /// Make `index` the active node of the working list. It is persisted
    /// only when the saved list holds the same node at that index.
    pub(super) fn set_working_active(&mut self, index: usize, now: Instant) -> Changes {
        self.gui.runtime.selected_profile = index;
        let same = self.saved.profiles.profiles.get(index).is_some()
            && self.saved.profiles.profiles.get(index) == self.gui.profiles.profiles.get(index);
        if same && self.saved.active != index {
            self.saved.active = index;
            return self.mark_dirty(ConfigFile::Runtime, now);
        }
        Changes::NONE
    }

    /// 儲存 / Ctrl+S: validate and write the working list now.
    pub fn save_nodes(&mut self, now: Instant) -> Changes {
        self.write_nodes(now).0
    }

    /// Save; the flag is false when nothing was written because a node is
    /// invalid or the write failed. Either failure cancels a pending leave.
    fn write_nodes(&mut self, now: Instant) -> (Changes, bool) {
        if !self.nodes_dirty() {
            return (Changes::NONE, true);
        }
        if let Some(notice) = self.save_refusal() {
            self.leave = None;
            return (Changes::NODES | self.set_notice(notice), false);
        }
        let mut next = self.saved.clone();
        next.profiles = self.gui.profiles.clone();
        next.active = self.gui.runtime.selected_profile;
        next.clamp_active();
        if let Err(err) = self.persist.write(ConfigFile::Profiles, &self.gui, &next) {
            self.leave = None;
            let notice = Notice::NodesSaveFailed(format!("{err:#}"));
            return (Changes::NODES | self.set_notice(notice), false);
        }
        let active_changed = next.active != self.saved.active;
        self.saved = next;
        let mut changes = Changes::NODES | self.clear_save_notice();
        if active_changed {
            changes |= self.mark_dirty(ConfigFile::Runtime, now);
        }
        (changes, true)
    }

    /// Why Save must refuse: invalid editor text, or a node that lacks
    /// mandatory fields (named in the notice).
    fn save_refusal(&mut self) -> Option<Notice> {
        self.nodes.errors = self.nodes.draft.errors();
        if !self.nodes.errors.is_empty() {
            return Some(Notice::SaveInvalid);
        }
        self.gui.profiles.profiles.iter().find_map(|profile| {
            let missing = missing_fields(profile);
            (!missing.is_empty()).then(|| Notice::NodeIncomplete(profile.display_name(), missing))
        })
    }

    fn clear_save_notice(&mut self) -> Changes {
        if matches!(
            self.notice,
            Notice::NodesSaveFailed(_)
                | Notice::SaveInvalid
                | Notice::NodeIncomplete(..)
                | Notice::ProfilesChanged
        ) {
            self.dismiss_notice()
        } else {
            Changes::NONE
        }
    }

    /// 還原: restore the saved list and active node, then reload the editor.
    pub fn revert_nodes(&mut self) -> Changes {
        if !self.nodes_dirty() {
            return Changes::NONE;
        }
        self.gui.profiles = self.saved.profiles.clone();
        self.gui.runtime.selected_profile = self.saved.active;
        self.gui.normalize_selected_index();
        self.nodes.advanced = None;
        self.nodes.advanced_errors.clear();
        Changes::OVERVIEW | self.load_selected() | self.clear_save_notice()
    }

    /// Closing the window or quitting. Returns the action to perform now,
    /// or `None` while the prompt asks first. A request made while the
    /// prompt is open is ignored and never replaces the stored one.
    pub fn request_leave(&mut self, leave: Leave) -> (Changes, Option<Leave>) {
        if self.leave.is_some() {
            return (Changes::NONE, None);
        }
        if !self.nodes_dirty() {
            return (Changes::NONE, Some(leave));
        }
        self.leave = Some(leave);
        (Changes::NODES, None)
    }

    /// Answer the prompt. Returns the stored action when it may proceed.
    pub fn answer_prompt(&mut self, answer: Answer, now: Instant) -> (Changes, Option<Leave>) {
        let Some(leave) = self.leave else {
            return (Changes::NONE, None);
        };
        match answer {
            Answer::Save => {
                let (changes, saved) = self.write_nodes(now);
                let leave = saved.then(|| self.leave.take()).flatten();
                (changes | Changes::NODES, leave)
            }
            Answer::Discard => {
                self.leave = None;
                (self.revert_nodes() | Changes::NODES, Some(leave))
            }
            Answer::Cancel => {
                self.leave = None;
                (Changes::NODES, None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::DraftField;
    use super::*;
    use crate::config::ProfileStore;

    const OSAKA: &str = "juicity://u:p@osaka.example.com:443?congestion_control=bbr#Osaka";

    fn disk(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("profiles.json")).unwrap()
    }

    fn runtime_index(dir: &std::path::Path) -> usize {
        std::fs::read(dir.join("runtime.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::config::RuntimeState>(&bytes).ok())
            .map_or(0, |runtime| runtime.selected_profile)
    }

    fn flush(c: &mut Controller) {
        c.flush_all(Instant::now()).unwrap();
    }

    #[test]
    fn every_mutation_leaves_disk_untouched_until_save() {
        let dir = temp_dir("save-untouched");
        let (mut c, fake) = controller(&dir);
        let before = disk(&dir);
        let now = Instant::now();
        fake.0.borrow_mut().paste = OSAKA.into();
        let steps: [&dyn Fn(&mut Controller) -> Changes; 8] = [
            &|c| c.edit_node(DraftField::Name, "東京 02"),
            &|c| c.edit_node(DraftField::Protocol, "shadowsocks"),
            &|c| c.edit_node(DraftField::Method, "aes-128-gcm"),
            &|c| c.add_node("Node 2".into()),
            &|c| c.node_command(0, super::super::ListCommand::Duplicate, now),
            &|c| c.node_command(1, super::super::ListCommand::MoveUp, now),
            &|c| c.node_command(0, super::super::ListCommand::Delete, now),
            &|c| c.import_links(),
        ];
        for step in steps {
            let _ = step(&mut c);
            flush(&mut c);
            assert_eq!(disk(&dir), before);
            assert!(c.nodes_dirty());
        }
        let _ = c.select_node(0);
        let _ = c.open_advanced();
        let _ = c.edit_advanced(DraftField::Sni, "front.example.com");
        let _ = c.commit_advanced();
        flush(&mut c);
        assert_eq!(disk(&dir), before);
        // Save writes the working list synchronously.
        let _ = c.revert_nodes();
        let _ = c.edit_node(DraftField::Name, "東京 02");
        assert!(c.nodes().dirty);
        let changes = c.save_nodes(now);
        assert!(changes.nodes);
        assert!(!c.nodes().dirty);
        assert!(disk(&dir).contains("東京 02"));
        // Settings and runtime writes never flush the working list.
        let _ = c.edit_node(DraftField::Name, "東京 03");
        let _ = c.set_close_to_tray(false, now);
        flush(&mut c);
        assert!(!disk(&dir).contains("東京 03"));
        c.shutdown();
        assert!(!disk(&dir).contains("東京 03"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_refuses_invalid_nodes_and_names_incomplete_ones() {
        let dir = temp_dir("save-invalid");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let before = disk(&dir);
        let _ = c.edit_node(DraftField::Port, "70000");
        let _ = c.edit_node(DraftField::Name, "東京 02");
        let _ = c.save_nodes(now);
        assert_eq!(c.notice().0, Notice::SaveInvalid);
        let _ = c.edit_node(DraftField::Port, "443");
        let _ = c.add_node("Node 2".into());
        // The new node is shown: its empty fields are flagged inline.
        let _ = c.save_nodes(now);
        assert_eq!(c.notice().0, Notice::SaveInvalid);
        assert!(!c.nodes().errors.is_empty());
        let _ = c.select_node(0);
        let _ = c.save_nodes(now);
        let Notice::NodeIncomplete(name, fields) = c.notice().0 else {
            panic!("save must name the incomplete node");
        };
        assert_eq!(name, "Node 2");
        assert!(!fields.is_empty());
        assert_eq!(disk(&dir), before);
        assert!(c.nodes_dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_keeps_dirty_and_cancels_the_leave() {
        let dir = temp_dir("save-fail");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let _ = c.edit_node(DraftField::Name, "東京 02");
        assert_eq!(c.request_leave(Leave::Quit).1, None);
        assert!(c.prompt_open());
        // A directory in place of profiles.json makes the final rename fail.
        std::fs::remove_file(dir.join("profiles.json")).unwrap();
        std::fs::create_dir(dir.join("profiles.json")).unwrap();
        let (changes, leave) = c.answer_prompt(Answer::Save, now);
        assert_eq!(leave, None, "the quit is cancelled");
        assert!(changes.notice && changes.nodes);
        assert!(!c.prompt_open());
        assert!(matches!(c.notice().0, Notice::NodesSaveFailed(_)));
        assert!(c.notice().0.is_error());
        assert!(c.nodes_dirty());
        assert_eq!(c.nodes().draft.name, "東京 02");
        // Once the disk is writable again, Save succeeds.
        std::fs::remove_dir(dir.join("profiles.json")).unwrap();
        let _ = c.save_nodes(now);
        assert!(!c.nodes_dirty());
        assert_eq!(c.notice().0, Notice::None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_triggers_and_answers() {
        for leave in [Leave::Hide, Leave::Quit] {
            for answer in [Answer::Save, Answer::Discard, Answer::Cancel] {
                let dir = temp_dir(&format!("save-prompt-{leave:?}-{answer:?}"));
                let (mut c, _) = controller(&dir);
                let now = Instant::now();
                // Clean: the action proceeds without asking.
                assert_eq!(c.request_leave(leave), (Changes::NONE, Some(leave)));
                let _ = c.edit_node(DraftField::Name, "東京 02");
                let (changes, now_leave) = c.request_leave(leave);
                assert!(changes.nodes && now_leave.is_none() && c.prompt_open());
                // A repeated request neither proceeds nor replaces the stored one.
                let other = if leave == Leave::Hide {
                    Leave::Quit
                } else {
                    Leave::Hide
                };
                assert_eq!(c.request_leave(other), (Changes::NONE, None));
                let serial_bump = c.answer_prompt(answer, now);
                assert!(!c.prompt_open());
                match answer {
                    Answer::Save => {
                        assert_eq!(serial_bump.1, Some(leave));
                        assert!(!serial_bump.0.editor);
                        assert!(disk(&dir).contains("東京 02"));
                        assert!(!c.nodes_dirty());
                    }
                    Answer::Discard => {
                        assert_eq!(serial_bump.1, Some(leave));
                        assert!(serial_bump.0.editor, "the editor reloads");
                        assert_eq!(c.nodes().draft.name, "Tokyo 01");
                        assert!(!disk(&dir).contains("東京 02"));
                        assert!(!c.nodes_dirty());
                    }
                    Answer::Cancel => {
                        assert_eq!(serial_bump.1, None);
                        assert!(!serial_bump.0.editor);
                        assert!(c.nodes_dirty());
                        assert_eq!(c.nodes().draft.name, "東京 02");
                    }
                }
                // No prompt pending: answers do nothing.
                assert_eq!(c.answer_prompt(answer, now), (Changes::NONE, None));
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    #[test]
    fn revert_restores_list_and_active_node() {
        let dir = temp_dir("save-revert");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        fake.0.borrow_mut().paste = OSAKA.into();
        let _ = c.import_links();
        let _ = c.set_active_node(now);
        assert_eq!(c.gui.runtime.selected_profile, 1);
        assert!(c.revert_nodes().editor);
        assert_eq!(c.node_count(), 1);
        assert_eq!(c.gui.runtime.selected_profile, 0);
        assert_eq!(c.nodes().selected, 0);
        assert!(!c.nodes_dirty());
        assert_eq!(c.revert_nodes(), Changes::NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_change_while_dirty_keeps_the_edits() {
        let dir = temp_dir("save-external");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let _ = c.edit_node(DraftField::Name, "東京 02");
        let external = r#"{ "profiles": [
            { "name": "Kyoto", "server": "kyoto.example.com", "uuid": "u", "password": "p" },
            { "name": "Nara", "server": "nara.example.com", "uuid": "u", "password": "p" }
        ] }"#;
        std::fs::write(dir.join("profiles.json"), external).unwrap();
        let changes = c.on_files_changed(now);
        assert!(changes.notice);
        assert_eq!(c.notice().0, Notice::ProfilesChanged);
        assert!(c.notice().0.is_error(), "the notice stays until dismissed");
        assert_eq!(c.nodes().draft.name, "東京 02");
        assert_eq!(c.node_count(), 1);
        assert_eq!(c.saved.profiles.profiles.len(), 2);
        // Revert now restores the external version.
        let _ = c.revert_nodes();
        assert_eq!(c.nodes().draft.name, "Kyoto");
        assert_eq!(c.node_count(), 2);
        assert_eq!(c.notice().0, Notice::None);
        // Clean: an external change replaces the working list.
        std::fs::write(dir.join("profiles.json"), TOKYO).unwrap();
        assert!(c.on_files_changed(now).editor);
        assert_eq!(c.node_count(), 1);
        assert!(!c.nodes_dirty());
        // Dirty again; Save overwrites the external change.
        let _ = c.edit_node(DraftField::Name, "東京 03");
        std::fs::write(dir.join("profiles.json"), external).unwrap();
        let _ = c.on_files_changed(now);
        let _ = c.save_nodes(now);
        assert!(disk(&dir).contains("東京 03") && !disk(&dir).contains("Kyoto"));
        // Our own write is not reported as an external change.
        assert_eq!(c.on_files_changed(now), Changes::NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const TOKYO: &str = r#"{ "profiles": [{ "name": "Tokyo 01", "server": "tokyo.example.com",
        "server_port": 443, "uuid": "u", "password": "p" }] }"#;

    #[test]
    fn starting_an_unsaved_node_writes_no_invalid_index() {
        let dir = temp_dir("save-start-unsaved");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        fake.0.borrow_mut().paste = OSAKA.into();
        let _ = c.import_links();
        let _ = c.set_active_node(now);
        let _ = c.toggle_connection(now);
        assert_eq!(
            fake.0.borrow().started,
            ["Osaka"],
            "start uses the working list"
        );
        flush(&mut c);
        let saved: ProfileStore = serde_json::from_str(&disk(&dir)).unwrap();
        assert_eq!(saved.profiles.len(), 1);
        assert_eq!(runtime_index(&dir), 0, "index 1 is not in the saved list");
        // After Save, the index is persisted with the list.
        let _ = c.save_nodes(now);
        flush(&mut c);
        assert_eq!(runtime_index(&dir), 1);
        c.shutdown();
        assert_eq!(runtime_index(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setting_an_unchanged_node_active_persists_at_once() {
        let dir = temp_dir("save-active-clean");
        std::fs::write(
            dir.join("profiles.json"),
            r#"{ "profiles": [
                { "name": "A", "server": "a.example.com", "uuid": "u", "password": "p" },
                { "name": "B", "server": "b.example.com", "uuid": "u", "password": "p" }
            ] }"#,
        )
        .unwrap();
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        // Another node has unsaved edits; B itself is unchanged.
        let _ = c.edit_node(DraftField::Name, "A2");
        let _ = c.select_node(1);
        assert!(c.set_active_node(now).persist);
        flush(&mut c);
        assert_eq!(runtime_index(&dir), 1);
        assert!(!disk(&dir).contains("A2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_serial_rules() {
        let dir = temp_dir("save-serial");
        let (mut c, fake) = controller(&dir);
        let now = Instant::now();
        // Typing, status and traffic updates never reload the editor.
        assert!(!c.edit_node(DraftField::Name, "東京 02").editor);
        assert!(!c.toggle_connection(now).editor);
        fake.0.borrow_mut().traffic = Some((1, 2));
        assert!(!c.poll_core().editor);
        assert!(!c.save_nodes(now).editor);
        // Revert, Don't Save and a node switch do.
        let _ = c.edit_node(DraftField::Name, "東京 03");
        assert!(c.revert_nodes().editor);
        let _ = c.edit_node(DraftField::Name, "東京 04");
        let _ = c.request_leave(Leave::Hide);
        assert!(c.answer_prompt(Answer::Discard, now).0.editor);
        let _ = c.add_node("Node 2".into());
        assert!(c.select_node(0).editor);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconnect_note_compares_the_saved_active_node() {
        let dir = temp_dir("save-reconnect");
        let (mut c, _) = controller(&dir);
        let now = Instant::now();
        let _ = c.toggle_connection(now);
        let _ = c.edit_node(DraftField::Server, "kix.example.com");
        assert!(
            !c.nodes().reconnect_required,
            "unsaved edits are not applied"
        );
        let _ = c.save_nodes(now);
        assert!(c.nodes().reconnect_required);
        let _ = c.toggle_connection(now);
        let _ = c.toggle_connection(now);
        assert!(!c.nodes().reconnect_required);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
