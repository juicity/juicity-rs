//! Nodes page callbacks and snapshot sync.

use super::{read, update};
use crate::config::{ProxyProtocol, SS_METHODS};
use crate::ui::controller::{DraftData, DraftError, DraftField, ListCommand, NodesSnapshot};
use crate::ui::{
    Actions, AppState, FieldError, FieldText, MainWindow, MenuItem, NodeCommand, NodeDraft,
    NodeField, NodeRow, NodeStore, Protocol,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

fn field_from(field: NodeField) -> DraftField {
    match field {
        NodeField::Name => DraftField::Name,
        NodeField::Protocol => DraftField::Protocol,
        NodeField::Server => DraftField::Server,
        NodeField::Port => DraftField::Port,
        NodeField::Uuid => DraftField::Uuid,
        NodeField::Password => DraftField::Password,
        NodeField::Method => DraftField::Method,
        NodeField::Sni => DraftField::Sni,
        NodeField::AllowInsecure => DraftField::AllowInsecure,
        NodeField::PinnedCertchainSha256 => DraftField::PinnedCertchainSha256,
        NodeField::CongestionControl => DraftField::CongestionControl,
        NodeField::Plugin => DraftField::Plugin,
        NodeField::PluginOptions => DraftField::PluginOptions,
        NodeField::PluginArgs => DraftField::PluginArgs,
        NodeField::Timeout => DraftField::Timeout,
        NodeField::Group => DraftField::Group,
    }
}

fn field_to(field: DraftField) -> NodeField {
    match field {
        DraftField::Name => NodeField::Name,
        DraftField::Protocol => NodeField::Protocol,
        DraftField::Server => NodeField::Server,
        DraftField::Port => NodeField::Port,
        DraftField::Uuid => NodeField::Uuid,
        DraftField::Password => NodeField::Password,
        DraftField::Method => NodeField::Method,
        DraftField::Sni => NodeField::Sni,
        DraftField::AllowInsecure => NodeField::AllowInsecure,
        DraftField::PinnedCertchainSha256 => NodeField::PinnedCertchainSha256,
        DraftField::CongestionControl => NodeField::CongestionControl,
        DraftField::Plugin => NodeField::Plugin,
        DraftField::PluginOptions => NodeField::PluginOptions,
        DraftField::PluginArgs => NodeField::PluginArgs,
        DraftField::Timeout => NodeField::Timeout,
        DraftField::Group => NodeField::Group,
    }
}

fn command_from(command: NodeCommand) -> ListCommand {
    match command {
        NodeCommand::MoveUp => ListCommand::MoveUp,
        NodeCommand::MoveDown => ListCommand::MoveDown,
        NodeCommand::Duplicate => ListCommand::Duplicate,
        NodeCommand::Delete => ListCommand::Delete,
    }
}

fn protocol_to(protocol: ProxyProtocol) -> Protocol {
    match protocol {
        ProxyProtocol::Juicity => Protocol::Juicity,
        ProxyProtocol::Shadowsocks => Protocol::Shadowsocks,
    }
}

fn draft_to(draft: &DraftData) -> NodeDraft {
    NodeDraft {
        name: draft.name.as_str().into(),
        protocol: protocol_to(draft.protocol),
        server: draft.server.as_str().into(),
        port: draft.port.as_str().into(),
        uuid: draft.uuid.as_str().into(),
        password: draft.password.as_str().into(),
        method: draft.method.as_str().into(),
        sni: draft.sni.as_str().into(),
        allow_insecure: draft.allow_insecure,
        pinned_certchain_sha256: draft.pinned_certchain_sha256.as_str().into(),
        congestion_control: draft.congestion_control.as_str().into(),
        plugin: draft.plugin.as_str().into(),
        plugin_options: draft.plugin_options.as_str().into(),
        plugin_args: draft.plugin_args.as_str().into(),
        timeout: draft.timeout.as_str().into(),
        group: draft.group.as_str().into(),
    }
}

/// Row index from Slint; negative means none.
fn index(row: i32) -> Option<usize> {
    usize::try_from(row).ok()
}

pub fn wire(ui: &MainWindow) {
    let methods: Vec<SharedString> = SS_METHODS.iter().map(|m| (*m).into()).collect();
    ui.global::<NodeStore>()
        .set_methods(ModelRc::new(VecModel::from(methods)));
    let actions = ui.global::<Actions>();
    actions.on_select_node(|row| {
        if let Some(i) = index(row) {
            update(|c, _| c.select_node(i));
        }
    });
    let weak = ui.as_weak();
    actions.on_add_node(move || {
        let Some(ui) = weak.upgrade() else { return };
        let count = read(|c| c.node_count()).unwrap_or(0);
        let number = i32::try_from(count + 1).unwrap_or(i32::MAX);
        let name = ui.global::<FieldText>().invoke_new_node_name(number);
        update(move |c, _| c.add_node(name.into()));
    });
    actions.on_import_links(|| update(|c, _| c.import_links()));
    actions.on_node_command(|row, command| {
        if let Some(i) = index(row) {
            update(|c, now| c.node_command(i, command_from(command), now));
        }
    });
    actions.on_edit_node(|field, value| {
        update(|c, _| c.edit_node(field_from(field), &value));
    });
    actions.on_set_active_node(|| update(|c, now| c.set_active_node(now)));
    actions.on_export_link(|| update(|c, _| c.export_link()));
    actions.on_open_advanced(|| update(|c, _| c.open_advanced()));
    actions.on_edit_advanced(|field, value| {
        update(|c, _| c.edit_advanced(field_from(field), &value));
    });
    actions.on_commit_advanced(|| update(|c, _| c.commit_advanced()));
    actions.on_save_nodes(|| update(|c, _| c.save_nodes()));
    actions.on_revert_nodes(|| update(|c, _| c.revert_nodes()));
    actions.on_cancel_advanced(|| update(|c, _| c.cancel_advanced()));
}

/// Update `model` in place so repeaters keep their instances (an open
/// context menu stays attached to its row).
fn sync_model<T: Clone + PartialEq + 'static>(
    model: ModelRc<T>,
    rows: Vec<T>,
) -> Option<ModelRc<T>> {
    let Some(vec) = model.as_any().downcast_ref::<VecModel<T>>() else {
        return Some(ModelRc::new(VecModel::from(rows)));
    };
    for (i, row) in rows.iter().enumerate() {
        if i >= vec.row_count() {
            vec.push(row.clone());
        } else if vec.row_data(i).as_ref() != Some(row) {
            vec.set_row_data(i, row.clone());
        }
    }
    while vec.row_count() > rows.len() {
        vec.remove(vec.row_count() - 1);
    }
    None
}

/// Push the nodes snapshot. The draft is always current (a page that is
/// rebuilt loads it); the editor text fields reload only when `editor`
/// bumps the serial (another node, a menu choice, a committed sheet, a
/// reload of the edited node), never while the user types.
pub fn sync(ui: &MainWindow, snapshot: &NodesSnapshot, editor: bool) {
    let store = ui.global::<NodeStore>();
    let rows = snapshot
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| NodeRow {
            id: i.to_string().into(),
            name: row.name.as_str().into(),
            address: row.address.as_str().into(),
            protocol: protocol_to(row.protocol),
            in_use: row.in_use,
            group: row.group.as_str().into(),
        })
        .collect();
    if let Some(model) = sync_model(store.get_rows(), rows) {
        store.set_rows(model);
    }
    store.set_selected(i32::try_from(snapshot.selected).unwrap_or(-1));
    store.set_draft(draft_to(&snapshot.draft));
    if editor {
        store.set_draft_serial(store.get_draft_serial().wrapping_add(1));
    }
    let methods = SS_METHODS
        .iter()
        .map(|method| MenuItem {
            text: (*method).into(),
            checked: *method == snapshot.draft.method,
            enabled: true,
            danger: false,
            font: Default::default(),
        })
        .collect();
    if let Some(model) = sync_model(store.get_method_items(), methods) {
        store.set_method_items(model);
    }

    let text = ui.global::<FieldText>();
    let message = |error: DraftError| match error {
        DraftError::Required => text.get_required_error(),
        DraftError::Port => text.get_port_error(),
        DraftError::Timeout => text.get_timeout_error(),
    };
    let error_for = |wanted: DraftField| {
        snapshot
            .errors
            .iter()
            .find(|(field, _)| *field == wanted)
            .map(|(_, error)| message(*error))
            .unwrap_or_default()
    };
    store.set_server_error(error_for(DraftField::Server));
    store.set_uuid_error(error_for(DraftField::Uuid));
    store.set_password_error(error_for(DraftField::Password));
    store.set_port_error(error_for(DraftField::Port));
    store.set_timeout_error(error_for(DraftField::Timeout));
    let errors: Vec<FieldError> = snapshot
        .errors
        .iter()
        .map(|(field, error)| FieldError {
            field: field_to(*field),
            message: message(*error),
        })
        .collect();
    store.set_errors(ModelRc::new(VecModel::from(errors)));
    store.set_reconnect_required(snapshot.reconnect_required);
    store.set_dirty(snapshot.dirty);
    ui.global::<AppState>().set_save_prompt(snapshot.prompt);
    match &snapshot.advanced {
        Some(sheet) => {
            store.set_advanced_draft(draft_to(sheet));
            // Sheet fields load once when it opens, never on later syncs.
            if !store.get_advanced_open() {
                store.set_advanced_serial(store.get_advanced_serial().wrapping_add(1));
                store.set_advanced_open(true);
            }
        }
        None => store.set_advanced_open(false),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::controller::testing::{controller, temp_dir};
    use super::super::{install, shutdown, update};
    use super::*;
    use crate::ui::{AppState, Page};
    use i_slint_backend_testing::ElementHandle;
    use std::time::Duration;

    const DEBOUNCE_WAIT: Duration = Duration::from_millis(450);

    fn setup(name: &str) -> (MainWindow, std::path::PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, _) = controller(&dir);
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        ui.global::<AppState>().set_page(Page::Nodes);
        ui.show().unwrap();
        // Run change handlers so the page and its fields exist.
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        (ui, dir)
    }

    fn field(ui: &MainWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(ui, label)
            .find(|element| element.type_name().is_some_and(|name| name == "TextInput"))
            .unwrap_or_else(|| panic!("no field labelled {label}"))
    }

    fn teardown(dir: &std::path::Path) {
        shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn click(ui: &MainWindow, x: f32, y: f32) {
        use slint::platform::{PointerEventButton, WindowEvent};
        let position = slint::LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        ui.window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    }

    /// Open the editor's overflow menu and click its `item`th entry.
    fn overflow(ui: &MainWindow, item: usize) {
        let more = ElementHandle::find_by_element_id(ui, "NodeEditor::more")
            .next()
            .expect("overflow button");
        let (at, size) = (more.absolute_position(), more.size());
        click(ui, at.x + size.width / 2.0, at.y + size.height / 2.0);
        // The 160 px menu opens below the button, right-aligned to it, with
        // 8 px padding and 44 px rows.
        let row_y = at.y + size.height + 8.0 + 44.0 * item as f32 + 22.0;
        click(ui, at.x + size.width - 80.0, row_y);
    }

    #[test]
    fn overflow_menu_duplicates_and_deletes() {
        let (ui, dir) = setup("bind-nodes-overflow");
        ui.window().set_size(slint::LogicalSize::new(960.0, 900.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        let store = ui.global::<NodeStore>();
        let count = store.get_rows().row_count();
        overflow(&ui, 0);
        assert_eq!(store.get_rows().row_count(), count + 1, "Duplicate");
        assert!(store.get_dirty());
        overflow(&ui, 1);
        assert_eq!(store.get_rows().row_count(), count, "Delete");
        teardown(&dir);
    }

    #[test]
    fn typing_is_saved_only_by_save() {
        let (ui, dir) = setup("bind-nodes-save");
        let store = ui.global::<NodeStore>();
        assert_eq!(field(&ui, "Name").accessible_value().unwrap(), "Tokyo 01");
        assert!(!store.get_dirty());
        field(&ui, "Name").set_accessible_value("Osaka 01");
        assert_eq!(ui.global::<AppState>().get_active_name(), "Osaka 01");
        assert!(store.get_dirty());
        let read = || std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        std::thread::sleep(DEBOUNCE_WAIT);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        assert!(!read().contains("Osaka 01"), "no autosave");
        let serial = store.get_draft_serial();
        ui.global::<Actions>().invoke_save_nodes();
        assert!(read().contains("Osaka 01"));
        assert!(!store.get_dirty());
        assert_eq!(store.get_draft_serial(), serial, "Save keeps the editor");
        assert!(!dir.join("profiles.tmp").exists());
        // Revert restores the saved list and reloads the editor.
        field(&ui, "Name").set_accessible_value("Kyoto 01");
        ui.global::<Actions>().invoke_revert_nodes();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(field(&ui, "Name").accessible_value().unwrap(), "Osaka 01");
        assert!(!store.get_dirty());
        teardown(&dir);
    }

    #[test]
    fn edits_survive_leaving_and_reopening_the_page() {
        let (ui, dir) = setup("bind-nodes-page");
        field(&ui, "Name").set_accessible_value("Osaka 01");
        field(&ui, "Port").set_accessible_value("70000");
        ui.global::<AppState>().set_page(Page::Overview);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert!(ElementHandle::find_by_accessible_label(&ui, "Port")
            .next()
            .is_none());
        ui.global::<AppState>().set_page(Page::Nodes);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(field(&ui, "Name").accessible_value().unwrap(), "Osaka 01");
        assert_eq!(field(&ui, "Port").accessible_value().unwrap(), "70000");
        teardown(&dir);
    }

    #[test]
    fn invalid_port_shows_an_error_and_is_not_saved() {
        let (ui, dir) = setup("bind-nodes-port");
        let before = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        field(&ui, "Port").set_accessible_value("70000");
        let store = ui.global::<NodeStore>();
        assert_eq!(store.get_port_error(), "Enter a port from 1 to 65535");
        std::thread::sleep(DEBOUNCE_WAIT);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        assert_eq!(
            std::fs::read_to_string(dir.join("profiles.json")).unwrap(),
            before
        );
        teardown(&dir);
    }

    fn press(ui: &MainWindow, text: &str, control: bool) {
        use slint::platform::{Key, WindowEvent};
        let window = ui.window();
        if control {
            window.dispatch_event(WindowEvent::KeyPressed {
                text: Key::Control.into(),
            });
        }
        window.dispatch_event(WindowEvent::KeyPressed { text: text.into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: text.into() });
        if control {
            window.dispatch_event(WindowEvent::KeyReleased {
                text: Key::Control.into(),
            });
        }
    }

    #[test]
    fn keyboard_saves_and_answers_the_prompt() {
        use slint::platform::Key;
        let (ui, dir) = setup("bind-nodes-keys");
        let store = ui.global::<NodeStore>();
        let state = ui.global::<AppState>();
        let read = || std::fs::read_to_string(dir.join("profiles.json")).unwrap();
        field(&ui, "Name").set_accessible_value("Osaka 01");
        press(&ui, "s", true);
        assert!(
            !store.get_dirty() && read().contains("Osaka 01"),
            "Ctrl+S saves"
        );
        // Esc cancels the prompt; Enter saves.
        field(&ui, "Name").set_accessible_value("Kyoto 01");
        update(|c, _| c.request_leave(crate::ui::controller::Leave::Hide).0);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert!(state.get_save_prompt());
        press(&ui, &SharedString::from(Key::Escape), false);
        assert!(!state.get_save_prompt() && store.get_dirty());
        update(|c, _| c.request_leave(crate::ui::controller::Leave::Hide).0);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        press(&ui, &SharedString::from(Key::Return), false);
        assert!(!state.get_save_prompt() && !store.get_dirty());
        assert!(read().contains("Kyoto 01"));
        teardown(&dir);
    }

    #[test]
    fn field_errors_follow_a_language_switch() {
        let (ui, dir) = setup("bind-nodes-language");
        field(&ui, "Port").set_accessible_value("70000");
        let store = ui.global::<NodeStore>();
        ui.global::<Actions>().invoke_set_language(4);
        assert_eq!(store.get_port_error(), "Введите порт от 1 до 65535");
        ui.global::<Actions>().invoke_set_language(1);
        assert_eq!(store.get_port_error(), "Enter a port from 1 to 65535");
        teardown(&dir);
    }

    #[test]
    fn sheet_cancel_discards_and_done_commits() {
        let (ui, dir) = setup("bind-nodes-sheet");
        let actions = ui.global::<Actions>();
        let store = ui.global::<NodeStore>();
        actions.invoke_open_advanced();
        assert!(store.get_advanced_open());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        field(&ui, "SNI").set_accessible_value("front.example.com");
        assert_eq!(store.get_advanced_draft().sni, "front.example.com");
        // Other sheet edits and background syncs keep the typed text.
        field(&ui, "Group").set_accessible_value("JP");
        actions.invoke_toggle_connection();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(
            field(&ui, "SNI").accessible_value().unwrap(),
            "front.example.com"
        );
        actions.invoke_cancel_advanced();
        assert!(!store.get_advanced_open());
        assert_eq!(store.get_draft().sni, "");
        // The closed sheet is gone; reopening starts from the saved value.
        assert!(ElementHandle::find_by_accessible_label(&ui, "SNI")
            .next()
            .is_none());
        actions.invoke_open_advanced();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(field(&ui, "SNI").accessible_value().unwrap(), "");
        actions.invoke_edit_advanced(NodeField::Sni, "front.example.com".into());
        actions.invoke_commit_advanced();
        assert!(!store.get_advanced_open());
        assert_eq!(store.get_draft().sni, "front.example.com");
        teardown(&dir);
    }

    #[test]
    fn background_sync_keeps_the_field_being_edited() {
        let (ui, dir) = setup("bind-nodes-background");
        let serial = ui.global::<NodeStore>().get_draft_serial();
        field(&ui, "Port").set_accessible_value("70000");
        // Core state, a reload of another file and a list update arrive.
        ui.global::<Actions>().invoke_toggle_connection();
        std::fs::write(dir.join("runtime.json"), r#"{ "close_to_tray": false }"#).unwrap();
        update(|c, now| c.on_files_changed(now));
        ui.global::<Actions>()
            .invoke_edit_node(NodeField::Name, "Tokyo 02".into());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(ui.global::<NodeStore>().get_draft_serial(), serial);
        assert_eq!(field(&ui, "Port").accessible_value().unwrap(), "70000");
        assert!(ui.global::<NodeStore>().get_dirty());
        // A refused Save keeps the typed text too.
        ui.global::<Actions>().invoke_save_nodes();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(ui.global::<NodeStore>().get_draft_serial(), serial);
        assert_eq!(field(&ui, "Port").accessible_value().unwrap(), "70000");
        assert_eq!(
            ui.global::<AppState>().get_notice(),
            crate::ui::Notice::SaveInvalid
        );
        teardown(&dir);
    }
}
