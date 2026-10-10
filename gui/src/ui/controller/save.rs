//! Explicit save for nodes: Save, Revert and the unsaved-changes prompt
//! shown before the window closes or the app quits.
//!
//! Every node edit changes only the working list (`gui.profiles`) and the
//! working active node (`gui.runtime.selected_profile`); disk holds `saved`
//! until Save writes both synchronously.

use super::{Changes, ConfigFile, Controller, Notice};
use crate::validate::missing_fields;

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
    /// The working list or active node differs from the saved ones.
    pub(super) fn list_dirty(&self) -> bool {
        self.gui.profiles != self.saved.profiles
            || self.gui.runtime.selected_profile != self.saved.active
    }

    /// Unsaved node edits: the working list or active node differs from
    /// the saved ones, or the editor holds invalid text it did not apply.
    pub fn nodes_dirty(&self) -> bool {
        self.list_dirty() || self.draft_unapplied()
    }

    /// The unsaved-changes prompt is waiting for an answer.
    pub fn prompt_open(&self) -> bool {
        self.leave.is_some()
    }

    /// Make `index` the active node of the working list. Only Save writes
    /// it; runtime writes keep the saved index.
    pub(super) fn set_working_active(&mut self, index: usize) {
        self.gui.runtime.selected_profile = index;
    }

    /// Save / Ctrl+S: validate and write the working list now.
    pub fn save_nodes(&mut self) -> Changes {
        self.write_nodes().0
    }

    /// Save; the flag is false unless both the list and the active index
    /// reached disk. Any failure keeps the edits dirty and cancels a
    /// pending leave.
    fn write_nodes(&mut self) -> (Changes, bool) {
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
        // The list first, so the index on disk never points past it.
        if next.profiles != self.saved.profiles {
            if let Err(err) = self.persist.write(ConfigFile::Profiles, &self.gui, &next) {
                return self.save_failed(err);
            }
            // `saved` follows what reached disk, even if the index fails.
            self.saved.profiles = next.profiles;
            self.saved.clamp_active();
        }
        if next.active != self.saved.active {
            let written = self.saved.active;
            self.saved.active = next.active;
            if let Err(err) = self
                .persist
                .write(ConfigFile::Runtime, &self.gui, &self.saved)
            {
                self.saved.active = written;
                return self.save_failed(err);
            }
        }
        (Changes::NODES | self.clear_save_notice(), true)
    }

    fn save_failed(&mut self, err: anyhow::Error) -> (Changes, bool) {
        self.leave = None;
        let notice = Notice::NodesSaveFailed(format!("{err:#}"));
        (Changes::NODES | self.set_notice(notice), false)
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

    /// Revert: restore the saved list and active node, select the active
    /// node and reload the editor, dropping invalid text.
    pub fn revert_nodes(&mut self) -> Changes {
        if !self.nodes_dirty() {
            return Changes::NONE;
        }
        self.gui.profiles = self.saved.profiles.clone();
        self.gui.runtime.selected_profile = self.saved.active;
        self.gui.normalize_selected_index();
        self.nodes.selected = self.gui.runtime.selected_profile;
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
    pub fn answer_prompt(&mut self, answer: Answer) -> (Changes, Option<Leave>) {
        let Some(leave) = self.leave else {
            return (Changes::NONE, None);
        };
        match answer {
            Answer::Save => {
                let (changes, saved) = self.write_nodes();
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
    use std::time::Instant;

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
        let changes = c.save_nodes();
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
        let before = disk(&dir);
        let _ = c.edit_node(DraftField::Port, "70000");
        let _ = c.edit_node(DraftField::Name, "東京 02");
        let _ = c.save_nodes();
        assert_eq!(c.notice().0, Notice::SaveInvalid);
        let _ = c.edit_node(DraftField::Port, "443");
        let _ = c.add_node("Node 2".into());
        // The new node is shown: its empty fields are flagged inline.
        let _ = c.save_nodes();
        assert_eq!(c.notice().0, Notice::SaveInvalid);
        assert!(!c.nodes().errors.is_empty());
        let _ = c.select_node(0);
        let _ = c.save_nodes();
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
        let _ = c.edit_node(DraftField::Name, "東京 02");
        assert_eq!(c.request_leave(Leave::Quit).1, None);
        assert!(c.prompt_open());
        // A directory in place of profiles.json makes the final rename fail.
        std::fs::remove_file(dir.join("profiles.json")).unwrap();
        std::fs::create_dir(dir.join("profiles.json")).unwrap();
        let (changes, leave) = c.answer_prompt(Answer::Save);
        assert_eq!(leave, None, "the quit is cancelled");
        assert!(changes.notice && changes.nodes);
        assert!(!c.prompt_open());
        assert!(matches!(c.notice().0, Notice::NodesSaveFailed(_)));
        assert!(c.notice().0.is_error());
        assert!(c.nodes_dirty());
        assert_eq!(c.nodes().draft.name, "東京 02");
        // Once the disk is writable again, Save succeeds.
        std::fs::remove_dir(dir.join("profiles.json")).unwrap();
        let _ = c.save_nodes();
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
                let serial_bump = c.answer_prompt(answer);
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
                assert_eq!(c.answer_prompt(answer), (Changes::NONE, None));
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
        let _ = c.save_nodes();
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
        let _ = c.save_nodes();
        flush(&mut c);
        assert_eq!(runtime_index(&dir), 1);
        c.shutdown();
        assert_eq!(runtime_index(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const A_B: &str = r#"{ "profiles": [
        { "name": "A", "server": "a.example.com", "uuid": "u", "password": "p" },
        { "name": "B", "server": "b.example.com", "uuid": "u", "password": "p" }
    ] }"#;

    fn a_b(name: &str) -> (std::path::PathBuf, Controller, FakeEffects) {
        let dir = temp_dir(name);
        std::fs::write(dir.join("profiles.json"), A_B).unwrap();
        let (c, fake) = controller(&dir);
        (dir, c, fake)
    }

    #[test]
    fn changing_the_active_node_waits_for_save() {
        let (dir, mut c, _) = a_b("save-active");
        let now = Instant::now();
        std::fs::write(dir.join("runtime.json"), r#"{ "selected_profile": 0 }"#).unwrap();
        // From the editor: only the working active node changes.
        let _ = c.select_node(1);
        let _ = c.set_active_node(now);
        assert!(c.nodes().rows[1].in_use);
        assert!(c.nodes_dirty(), "a new active node alone is unsaved");
        flush(&mut c);
        assert_eq!(runtime_index(&dir), 0);
        // Don't Save restores the saved active node.
        let _ = c.request_leave(Leave::Hide);
        let _ = c.answer_prompt(Answer::Discard);
        assert_eq!(c.gui.runtime.selected_profile, 0);
        assert!(!c.nodes_dirty());
        // From the tray: the same, until Save writes the index.
        let _ = c.on_tray(crate::desktop::tray::TrayEvent::SelectNode(1), now);
        assert!(c.nodes_dirty());
        let _ = c.toggle_connection(now);
        flush(&mut c);
        assert_eq!(
            runtime_index(&dir),
            0,
            "runtime writes keep the saved index"
        );
        assert!(!c.save_nodes().persist, "Save writes at once");
        assert_eq!(runtime_index(&dir), 1);
        assert!(!c.nodes_dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_list_and_active_index_or_stays_dirty() {
        let dir = temp_dir("save-atomic");
        let (mut c, fake) = controller(&dir);
        fake.0.borrow_mut().paste = OSAKA.into();
        let _ = c.import_links();
        let _ = c.set_active_node(Instant::now());
        assert_eq!(c.request_leave(Leave::Quit).1, None);
        // profiles.json is writable, runtime.json is not.
        std::fs::create_dir(dir.join("runtime.json")).unwrap();
        let (changes, leave) = c.answer_prompt(Answer::Save);
        assert_eq!(leave, None, "the quit is cancelled");
        assert!(changes.notice);
        assert!(matches!(c.notice().0, Notice::NodesSaveFailed(_)));
        assert!(!c.prompt_open());
        // `saved` holds what reached disk: the new list, the old index.
        assert!(disk(&dir).contains("Osaka"));
        assert_eq!(c.saved.profiles, c.gui.profiles);
        assert_eq!(c.saved.active, 0);
        assert!(c.nodes_dirty());
        std::fs::remove_dir(dir.join("runtime.json")).unwrap();
        let _ = c.save_nodes();
        assert_eq!(runtime_index(&dir), 1);
        assert!(!c.nodes_dirty());
        assert_eq!(c.notice().0, Notice::None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_text_is_unsaved_until_reverted() {
        let (dir, mut c, _) = a_b("save-invalid-text");
        let before = disk(&dir);
        for (field, text) in [(DraftField::Port, "70000"), (DraftField::Server, "")] {
            let _ = c.edit_node(field, text);
            assert_eq!(c.gui.profiles, c.saved.profiles, "the text is not applied");
            assert!(c.nodes().dirty);
            assert_eq!(c.request_leave(Leave::Quit).1, None, "closing asks first");
            let _ = c.answer_prompt(Answer::Cancel);
            // Save refuses and shows the field errors.
            let _ = c.save_nodes();
            assert_eq!(c.notice().0, Notice::SaveInvalid);
            assert!(!c.nodes().errors.is_empty());
            assert_eq!(disk(&dir), before);
            // Revert drops the text.
            assert!(c.revert_nodes().editor);
            assert!(c.nodes().errors.is_empty());
            assert!(!c.nodes().dirty);
        }
        assert_eq!(c.nodes().draft.port, "443");
        assert_eq!(c.nodes().draft.server, "a.example.com");
        // Switching nodes drops invalid text of the node left behind.
        let _ = c.edit_node(DraftField::Port, "70000");
        let _ = c.select_node(1);
        assert!(!c.nodes().dirty);
        let _ = c.select_node(0);
        assert_eq!(c.nodes().draft.port, "443");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_and_dont_save_select_the_saved_active_node() {
        for discard in [false, true] {
            let (dir, mut c, _) = a_b(&format!("save-revert-select-{discard}"));
            let _ = c.select_node(1);
            let _ = c.edit_node(DraftField::Name, "B2");
            let changes = if discard {
                let _ = c.request_leave(Leave::Hide);
                c.answer_prompt(Answer::Discard).0
            } else {
                c.revert_nodes()
            };
            assert!(changes.editor, "the draft serial is bumped");
            assert_eq!(c.nodes().selected, 0);
            assert_eq!(c.nodes().draft.name, "A");
            assert!(c.nodes().errors.is_empty());
            assert!(!c.nodes_dirty());
            let _ = std::fs::remove_dir_all(&dir);
        }
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
        assert!(!c.save_nodes().editor);
        // Revert, Don't Save and a node switch do.
        let _ = c.edit_node(DraftField::Name, "東京 03");
        assert!(c.revert_nodes().editor);
        let _ = c.edit_node(DraftField::Name, "東京 04");
        let _ = c.request_leave(Leave::Hide);
        assert!(c.answer_prompt(Answer::Discard).0.editor);
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
        let _ = c.save_nodes();
        assert!(c.nodes().reconnect_required);
        let _ = c.toggle_connection(now);
        let _ = c.toggle_connection(now);
        assert!(!c.nodes().reconnect_required);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
