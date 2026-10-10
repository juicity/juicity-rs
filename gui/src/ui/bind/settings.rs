//! Settings page callbacks and snapshot sync.

use super::update;
use crate::config::{AppearancePreference, LanguagePreference, PacMode, StartupConnectionState};
use crate::ui::controller::{
    is_interval_preset, url_summary, SettingError, SettingKey, SettingsSnapshot,
};
use crate::ui::{
    Actions, FieldText, MainWindow, PacSource, SettingField, SettingsStore, StartupConnection,
    Theme,
};
use slint::ComponentHandle;

fn key_from(field: SettingField) -> SettingKey {
    match field {
        SettingField::MixedPort => SettingKey::MixedPort,
        SettingField::DirectUrl => SettingKey::DirectUrl,
        SettingField::ProxyUrl => SettingKey::ProxyUrl,
        SettingField::UpdateHours => SettingKey::UpdateHours,
        SettingField::PacListen => SettingKey::PacListen,
        SettingField::OnlinePacUrl => SettingKey::OnlinePacUrl,
    }
}

fn key_to(key: SettingKey) -> SettingField {
    match key {
        SettingKey::MixedPort => SettingField::MixedPort,
        SettingKey::DirectUrl => SettingField::DirectUrl,
        SettingKey::ProxyUrl => SettingField::ProxyUrl,
        SettingKey::UpdateHours => SettingField::UpdateHours,
        SettingKey::PacListen => SettingField::PacListen,
        SettingKey::OnlinePacUrl => SettingField::OnlinePacUrl,
    }
}

fn startup_from(state: StartupConnection) -> StartupConnectionState {
    match state {
        StartupConnection::Off => StartupConnectionState::Off,
        StartupConnection::On => StartupConnectionState::On,
        StartupConnection::Last => StartupConnectionState::LastState,
    }
}

fn startup_to(state: StartupConnectionState) -> StartupConnection {
    match state {
        StartupConnectionState::Off => StartupConnection::Off,
        StartupConnectionState::On => StartupConnection::On,
        StartupConnectionState::LastState => StartupConnection::Last,
    }
}

fn source_from(source: PacSource) -> PacMode {
    match source {
        PacSource::Local => PacMode::Local,
        PacSource::Online => PacMode::Online,
    }
}

fn source_to(mode: PacMode) -> PacSource {
    match mode {
        PacMode::Local => PacSource::Local,
        PacMode::Online => PacSource::Online,
    }
}

fn language_from(index: i32) -> LanguagePreference {
    match index {
        1 => LanguagePreference::En,
        2 => LanguagePreference::ZhCn,
        3 => LanguagePreference::ZhTw,
        4 => LanguagePreference::Ru,
        _ => LanguagePreference::FollowSystem,
    }
}

fn language_to(language: LanguagePreference) -> i32 {
    match language {
        LanguagePreference::FollowSystem => 0,
        LanguagePreference::En => 1,
        LanguagePreference::ZhCn => 2,
        LanguagePreference::ZhTw => 3,
        LanguagePreference::Ru => 4,
    }
}

fn appearance_from(index: i32) -> AppearancePreference {
    match index {
        1 => AppearancePreference::Light,
        2 => AppearancePreference::Dark,
        _ => AppearancePreference::FollowSystem,
    }
}

fn appearance_to(appearance: AppearancePreference) -> i32 {
    match appearance {
        AppearancePreference::FollowSystem => 0,
        AppearancePreference::Light => 1,
        AppearancePreference::Dark => 2,
    }
}

pub fn wire(ui: &MainWindow) {
    let actions = ui.global::<Actions>();
    actions
        .on_set_language(|index| update(move |c, now| c.set_language(language_from(index), now)));
    actions.on_set_appearance(|index| {
        update(move |c, now| c.set_appearance(appearance_from(index), now))
    });
    actions.on_set_autostart(|on| update(move |c, now| c.set_autostart(on, now)));
    actions.on_set_hide_on_start(|on| update(move |c, now| c.set_hide_on_start(on, now)));
    actions.on_set_close_to_tray(|on| update(move |c, now| c.set_close_to_tray(on, now)));
    actions.on_set_startup_connection(|state| {
        update(move |c, now| c.set_startup_connection(startup_from(state), now))
    });
    actions.on_set_pac_source(|source| {
        update(move |c, now| c.set_pac_source(source_from(source), now))
    });
    actions.on_set_update_hours(|hours| {
        update(move |c, now| c.set_update_hours(u32::try_from(hours).unwrap_or(0), now))
    });
    actions.on_open_setting(|field| update(move |c, _| c.open_setting(key_from(field))));
    actions.on_edit_setting(|text| update(move |c, _| c.edit_setting(&text)));
    actions.on_commit_setting(|| update(|c, now| c.commit_setting(now)));
    actions.on_cancel_setting(|| update(|c, _| c.cancel_setting()));

    ui.global::<SettingsStore>()
        .set_autostart_supported(crate::desktop::autostart::SUPPORTED);
    let versions = crate::version::Versions::current();
    ui.global::<SettingsStore>()
        .set_versions(crate::ui::Versions {
            gui: versions.app.into(),
            juicity: versions.juicity.into(),
            shadowsocks: versions.shadowsocks.into(),
            tag: versions.tag.into(),
            commit: crate::version::short_commit(versions.commit).into(),
        });
}

/// Push the settings snapshot. The sheet field reads `sheet-draft` only
/// when the sheet opens (`sheet-serial`), so later syncs never reset typed
/// text.
pub fn sync(ui: &MainWindow, snapshot: &SettingsSnapshot) {
    let store = ui.global::<SettingsStore>();
    store.set_language(language_to(snapshot.language));
    let theme = ui.global::<Theme>();
    let appearance = appearance_to(snapshot.appearance);
    if theme.get_appearance() != appearance {
        theme.set_appearance(appearance);
        super::desktop::apply_window_theme(ui);
    }
    store.set_autostart(snapshot.autostart);
    store.set_hide_on_start(snapshot.hide_on_start);
    store.set_close_to_tray(snapshot.close_to_tray);
    store.set_startup_connection(startup_to(snapshot.startup));
    store.set_mixed_port(snapshot.mixed_port.as_str().into());
    store.set_direct_url(url_summary(&snapshot.direct_url).into());
    store.set_proxy_url(url_summary(&snapshot.proxy_url).into());
    store.set_update_hours(i32::try_from(snapshot.update_hours).unwrap_or(i32::MAX));
    store.set_update_custom(!is_interval_preset(snapshot.update_hours));
    store.set_pac_source(source_to(snapshot.pac_source));
    store.set_pac_listen(snapshot.pac_listen.as_str().into());
    store.set_online_pac_url(snapshot.online_pac_url.as_str().into());
    match &snapshot.sheet {
        Some(sheet) => {
            let text = ui.global::<FieldText>();
            let error = match sheet.error {
                None => Default::default(),
                Some(SettingError::Port) => text.get_port_error(),
                Some(SettingError::ListenAddress) => text.get_listen_error(),
                Some(SettingError::Url) => text.get_url_error(),
                Some(SettingError::Hours) => text.get_hours_error(),
            };
            store.set_sheet_field(key_to(sheet.key));
            store.set_sheet_draft(sheet.draft.as_str().into());
            store.set_sheet_error(error);
            // The field loads the draft once per open, never on later syncs.
            if !store.get_sheet_open() {
                store.set_sheet_serial(store.get_sheet_serial().wrapping_add(1));
                store.set_sheet_open(true);
            }
        }
        None => store.set_sheet_open(false),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::controller::testing::{controller, temp_dir, FakeEffects};
    use super::super::{install, shutdown, update};
    use super::*;
    use crate::ui::{AppState, Page};
    use i_slint_backend_testing::ElementHandle;
    use std::time::Duration;

    const DEBOUNCE_WAIT: Duration = Duration::from_millis(450);

    fn setup(name: &str) -> (MainWindow, FakeEffects, std::path::PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, fake) = controller(&dir);
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        ui.global::<AppState>().set_page(Page::Settings);
        ui.show().unwrap();
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        (ui, fake, dir)
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

    #[test]
    fn page_shows_the_config() {
        let (ui, _, dir) = setup("bind-settings-page");
        let store = ui.global::<SettingsStore>();
        assert_eq!(store.get_mixed_port(), "1080");
        assert_eq!(store.get_direct_url(), "Loyalsoldier · direct-list.txt");
        assert_eq!(store.get_pac_listen(), "127.0.0.1:0");
        assert_eq!(store.get_pac_source(), PacSource::Local);
        assert!(store.get_close_to_tray());
        assert!(!store.get_versions().gui.is_empty());
        assert!(!store.get_sheet_open());
        teardown(&dir);
    }

    #[test]
    fn appearance_switches_theme_and_persists_live() {
        let (ui, _, dir) = setup("bind-settings-appearance");
        let actions = ui.global::<Actions>();
        let theme = ui.global::<Theme>();
        actions.invoke_set_appearance(2);
        assert!(theme.get_dark());
        std::thread::sleep(DEBOUNCE_WAIT);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        let saved: crate::config::RuntimeState =
            serde_json::from_slice(&std::fs::read(dir.join("runtime.json")).unwrap()).unwrap();
        assert_eq!(saved.appearance, AppearancePreference::Dark);
        actions.invoke_set_appearance(1);
        assert!(!theme.get_dark());
        assert_eq!(theme.get_appearance(), 1);
        teardown(&dir);
    }

    #[test]
    fn switches_and_menus_reach_the_controller() {
        let (ui, fake, dir) = setup("bind-settings-switches");
        let actions = ui.global::<Actions>();
        let store = ui.global::<SettingsStore>();
        assert_eq!(
            store.get_autostart_supported(),
            crate::desktop::autostart::SUPPORTED
        );
        #[cfg(target_os = "linux")]
        {
            actions.invoke_set_autostart(true);
            assert!(store.get_autostart());
            assert_eq!(fake.0.borrow().autostart, [true]);
            // A failed autostart update flips the switch back.
            fake.0.borrow_mut().autostart_error = Some("read-only".into());
            store.set_autostart(false);
            actions.invoke_set_autostart(false);
            assert!(store.get_autostart());
        }
        actions.invoke_set_startup_connection(StartupConnection::Last);
        assert_eq!(store.get_startup_connection(), StartupConnection::Last);
        actions.invoke_set_pac_source(PacSource::Online);
        assert_eq!(store.get_pac_source(), PacSource::Online);
        actions.invoke_set_update_hours(24);
        assert!(!store.get_update_custom());
        actions.invoke_set_update_hours(48);
        assert_eq!(store.get_update_hours(), 48);
        assert!(store.get_update_custom());
        teardown(&dir);
    }

    #[test]
    fn sheet_cancel_discards_and_done_commits_and_persists() {
        let (ui, _, dir) = setup("bind-settings-sheet");
        let actions = ui.global::<Actions>();
        let store = ui.global::<SettingsStore>();
        actions.invoke_open_setting(SettingField::MixedPort);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert!(store.get_sheet_open());
        assert_eq!(field(&ui, "Port").accessible_value().unwrap(), "1080");
        field(&ui, "Port").set_accessible_value("2080");
        actions.invoke_cancel_setting();
        assert!(!store.get_sheet_open());
        assert_eq!(store.get_mixed_port(), "1080");
        actions.invoke_open_setting(SettingField::MixedPort);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(field(&ui, "Port").accessible_value().unwrap(), "1080");
        field(&ui, "Port").set_accessible_value("70000");
        assert_eq!(store.get_sheet_error(), "Enter a port from 1 to 65535");
        actions.invoke_commit_setting();
        assert!(store.get_sheet_open(), "完成 refuses an invalid value");
        field(&ui, "Port").set_accessible_value("2080");
        assert_eq!(store.get_sheet_error(), "");
        actions.invoke_commit_setting();
        assert!(!store.get_sheet_open());
        assert_eq!(store.get_mixed_port(), "2080");
        assert_eq!(ui.global::<AppState>().get_local_port(), "2080");
        std::thread::sleep(DEBOUNCE_WAIT);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        let saved = std::fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(saved.contains("127.0.0.1:2080"), "{saved}");
        teardown(&dir);
    }

    #[test]
    fn background_sync_keeps_the_sheet_being_typed_in() {
        let (ui, _, dir) = setup("bind-settings-background");
        let actions = ui.global::<Actions>();
        actions.invoke_open_setting(SettingField::DirectUrl);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        field(&ui, "URL").set_accessible_value("https://mirror.example");
        let serial = ui.global::<SettingsStore>().get_sheet_serial();
        // Core state and an external edit of app.json arrive.
        actions.invoke_toggle_connection();
        std::fs::write(
            dir.join("app.json"),
            r#"{ "pac_listen": "127.0.0.1:0", "pac_auto_update_hours": 48 }"#,
        )
        .unwrap();
        update(|c, now| c.on_files_changed(now));
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert_eq!(ui.global::<SettingsStore>().get_update_hours(), 48);
        assert!(ui.global::<SettingsStore>().get_sheet_open());
        // The field reloads only on a new open, never on a background sync.
        assert_eq!(ui.global::<SettingsStore>().get_sheet_serial(), serial);
        assert_eq!(
            field(&ui, "URL").accessible_value().unwrap(),
            "https://mirror.example"
        );
        teardown(&dir);
    }
}
