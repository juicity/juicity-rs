//! Tray, window visibility, close-to-tray, the unsaved-changes prompt and
//! activation by a second launch.

use super::super::controller::{Answer, Leave};
use super::super::{Actions, AppState, MainWindow, Page, Theme, TrayText};
use super::{logs, read, spawn_rules, update_with, WINDOW};
use crate::desktop::single_instance::Activation;
use crate::desktop::tray::{Sink, Tray, TrayEvent, TrayLabels, TrayMenu};
use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::window::Theme as WindowTheme;
use slint::winit_030::WinitWindowAccessor;
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

/// How long hide-on-start waits for a tray icon before showing the window.
const TRAY_WAIT: Duration = Duration::from_secs(5);

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    static LAST_MENU: RefCell<Option<TrayMenu>> = const { RefCell::new(None) };
    static ACTIVATION: RefCell<Activation> = RefCell::new(Activation::default());
    static START_TIMER: slint::Timer = slint::Timer::default();
    // Where the window was when last hidden; hiding drops the native window.
    static LAST_POSITION: Cell<Option<PhysicalPosition<i32>>> = const { Cell::new(None) };
}

fn window() -> Option<MainWindow> {
    WINDOW.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade()))
}

/// The controller's tray model plus the translated labels.
fn menu(ui: &MainWindow) -> Option<TrayMenu> {
    let model = read(|c| c.tray_model())?;
    let text = ui.global::<TrayText>();
    let labels = TrayLabels {
        status: text.get_status().into(),
        proxy_status: text.get_proxy_status().into(),
        connect: text.get_connect().into(),
        system_proxy: text.get_system_proxy().into(),
        off: text.get_off().into(),
        pac: text.get_pac().into(),
        global: text.get_global().into(),
        bypass_china: text.get_bypass_china().into(),
        gfw_list: text.get_gfw_list().into(),
        update_rules: text.get_update_rules().into(),
        nodes: text.get_nodes().into(),
        import_clipboard: text.get_import_clipboard().into(),
        open: text.get_open().into(),
        edit_nodes: text.get_edit_nodes().into(),
        logs: text.get_logs().into(),
        settings: text.get_settings().into(),
        about: text.get_about().into(),
        quit: text.get_quit().into(),
    };
    Some(TrayMenu { model, labels })
}

/// Wire the unsaved-changes prompt.
pub(super) fn wire(ui: &MainWindow) {
    let actions = ui.global::<Actions>();
    actions.on_prompt_save(|| answer(Answer::Save));
    actions.on_prompt_discard(|| answer(Answer::Discard));
    actions.on_prompt_cancel(|| answer(Answer::Cancel));
}

/// Ask the controller whether `leave` may proceed; `None` while the prompt
/// asks first (or the request was ignored).
fn request(leave: Leave) -> Option<Leave> {
    update_with(|c, _| c.request_leave(leave)).flatten()
}

fn answer(answer: Answer) {
    if let Some(Some(leave)) = update_with(|c, _| c.answer_prompt(answer)) {
        perform(leave);
    }
}

fn perform(leave: Leave) {
    match leave {
        Leave::Hide => hide_window(),
        Leave::Quit => {
            // `ui::run` then calls `bind::shutdown`.
            let _ = slint::quit_event_loop();
        }
    }
}

/// Tray Quit, or Quit (Cmd-Q) in the macOS application menu. With unsaved node edits the prompt asks first; a
/// hidden window is shown so the prompt can be seen.
pub(super) fn request_quit() {
    match request(Leave::Quit) {
        Some(leave) => perform(leave),
        None => show_window(None),
    }
}

/// Create the tray from inside the event loop (tray-icon needs the running
/// main-thread loop on Windows and macOS).
pub fn start_tray() {
    let queued = slint::invoke_from_event_loop(|| {
        let Some(ui) = window() else {
            return;
        };
        let Some(menu) = menu(&ui) else {
            return;
        };
        let sink: Sink = Arc::new(|event| {
            if let Err(err) = slint::invoke_from_event_loop(move || handle(event)) {
                tracing::warn!("event loop is gone: {err}");
            }
        });
        let tray = Tray::start(menu.clone(), sink);
        LAST_MENU.with(|last| *last.borrow_mut() = Some(menu));
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
    });
    if let Err(err) = queued {
        tracing::warn!("system tray not started: {err}");
    }
}

/// Send the current menu to the tray when it changed (or always, `force`).
pub(super) fn push_tray(ui: &MainWindow, force: bool) {
    if TRAY.with(|t| t.borrow().is_none()) {
        return;
    }
    let Some(menu) = menu(ui) else {
        return;
    };
    let changed = LAST_MENU.with(|last| {
        let mut last = last.borrow_mut();
        if !force && last.as_ref() == Some(&menu) {
            return false;
        }
        *last = Some(menu.clone());
        true
    });
    if changed {
        TRAY.with(|t| {
            if let Some(tray) = t.borrow().as_ref() {
                tray.update(menu);
            }
        });
    }
}

/// Remove the tray icon and stop the startup timer.
pub(super) fn stop() {
    START_TIMER.with(|t| t.stop());
    TRAY.with(|t| t.borrow_mut().take());
    LAST_MENU.with(|last| last.borrow_mut().take());
}

fn tray_available() -> bool {
    TRAY.with(|t| t.borrow().as_ref().is_some_and(Tray::is_available))
}

/// A tray click, on the UI thread.
fn handle(event: TrayEvent) {
    match event {
        TrayEvent::ToggleWindow => toggle_window(),
        TrayEvent::Open => show_window(None),
        TrayEvent::ShowNodes => show_window(Some(Page::Nodes)),
        TrayEvent::ShowLogs => show_window(Some(Page::Logs)),
        // About is the last section of the settings page.
        TrayEvent::ShowSettings => show_window(Some(Page::Settings)),
        TrayEvent::ShowAbout => {
            if let Some(ui) = window() {
                ui.global::<AppState>().set_reveal_about(true);
            }
            show_window(Some(Page::Settings));
        }
        TrayEvent::Quit => request_quit(),
        _ => {
            if let Some(Some(job)) = update_with(|c, now| c.on_tray(event, now)) {
                spawn_rules(job);
            }
        }
    }
    // A native check item toggles itself when clicked; redraw from the model.
    if let Some(ui) = window() {
        push_tray(&ui, true);
    }
}

fn show_window(page: Option<Page>) {
    let Some(ui) = window() else {
        return;
    };
    if let Some(page) = page {
        ui.global::<Actions>().invoke_navigate(page);
    }
    let hidden = !ui.window().is_visible();
    if let Err(err) = ui.show() {
        tracing::warn!("could not show the window: {err}");
        return;
    }
    if hidden {
        place_window(&ui);
        apply_window_theme(&ui);
    }
    ui.window().set_minimized(false);
    logs::update_refresh();
    // No effect on Wayland, where the compositor decides focus.
    ui.window().with_winit_window(|w| w.focus_window());
}

fn hide_window() {
    let Some(ui) = window() else {
        return;
    };
    let position = ui
        .window()
        .with_winit_window(|w| w.outer_position().ok())
        .flatten();
    LAST_POSITION.with(|last| last.set(position));
    if let Err(err) = ui.hide() {
        tracing::warn!("could not hide the window: {err}");
    }
    logs::stop();
}

/// Put a newly shown window where it was last hidden, or centre the first one
/// on its monitor: the default cascade can push it past a small screen's edge.
/// Wayland ignores both, as the compositor places windows.
fn place_window(ui: &MainWindow) {
    let last = LAST_POSITION.with(Cell::take);
    ui.window().with_winit_window(|w| {
        let position = last.or_else(|| {
            let monitor = w.current_monitor().or_else(|| w.primary_monitor())?;
            let (origin, screen, outer) = (monitor.position(), monitor.size(), w.outer_size());
            let centre = |screen: u32, outer: u32| (screen.saturating_sub(outer) / 2) as i32;
            Some(PhysicalPosition::new(
                origin.x + centre(screen.width, outer.width),
                origin.y + centre(screen.height, outer.height),
            ))
        });
        if let Some(position) = position {
            w.set_outer_position(position);
        }
    });
}

/// Match the native title bar to the appearance setting once the native
/// window exists; hiding drops it, so every show applies it again.
pub(super) fn apply_window_theme(ui: &MainWindow) {
    let ui = ui.as_weak();
    let task = slint::spawn_local(async move {
        let Some(ui) = ui.upgrade() else {
            return;
        };
        let Ok(window) = ui.window().winit_window().await else {
            return;
        };
        // Read when the window arrives, so a later choice is never undone.
        window.set_theme(match ui.global::<Theme>().get_appearance() {
            1 => Some(WindowTheme::Light),
            2 => Some(WindowTheme::Dark),
            _ => None,
        });
    });
    if let Err(err) = task {
        tracing::debug!("could not set the window theme: {err}");
    }
}

fn toggle_window() {
    let Some(ui) = window() else {
        return;
    };
    if ui.window().is_visible() {
        hide_window();
    } else {
        show_window(None);
    }
}

/// Closing hides to the tray when close-to-tray is on and an icon is
/// shown; otherwise it quits.
fn quits_on_close(close_to_tray: bool, tray_available: bool) -> bool {
    !(close_to_tray && tray_available)
}

/// The window stays shown while the unsaved-changes prompt asks.
fn close_requested() -> slint::CloseRequestResponse {
    close_with_tray(tray_available())
}

fn close_with_tray(tray_available: bool) -> slint::CloseRequestResponse {
    let close_to_tray = read(|c| c.close_to_tray()).unwrap_or(false);
    let leave = if quits_on_close(close_to_tray, tray_available) {
        Leave::Quit
    } else {
        Leave::Hide
    };
    match request(leave) {
        Some(Leave::Quit) => {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
        // Hide like the tray does, so the logs timer stops now rather than
        // at its next tick.
        Some(Leave::Hide) => {
            hide_window();
            slint::CloseRequestResponse::HideWindow
        }
        None => slint::CloseRequestResponse::KeepWindowShown,
    }
}

/// Wire closing, then show the window unless hide-on-start is set. A hidden
/// start shows the window anyway when no tray icon appears in time.
pub fn show_initial(ui: &MainWindow) -> Result<(), slint::PlatformError> {
    ui.window().on_close_requested(close_requested);
    if !read(|c| c.hide_on_start()).unwrap_or(false) {
        ui.show()?;
        place_window(ui);
        apply_window_theme(ui);
        logs::update_refresh();
        return Ok(());
    }
    START_TIMER.with(|timer| {
        timer.start(slint::TimerMode::SingleShot, TRAY_WAIT, || {
            if !tray_available() {
                tracing::warn!("tray icon unavailable; showing the main window");
                show_window(None);
            }
        })
    });
    Ok(())
}

/// Another launch asked for the window (posted to the UI thread).
pub fn request_activation() {
    if ACTIVATION.with(|a| a.borrow_mut().request()) {
        show_window(None);
    }
}

/// The window exists; deliver an activation that arrived earlier.
pub fn window_ready() {
    if ACTIVATION.with(|a| a.borrow_mut().window_ready()) {
        show_window(None);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::controller::testing::{controller, temp_dir};
    use super::super::super::ProxyMode;
    use super::super::{install, shutdown, update};
    use super::*;

    fn setup(name: &str) -> (MainWindow, std::path::PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, _) = controller(&dir);
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        (ui, dir)
    }

    #[test]
    fn tray_menu_reads_translated_labels_and_model() {
        let (ui, dir) = setup("desktop-menu");
        let menu = menu(&ui).unwrap();
        assert_eq!(menu.model.nodes, ["Tokyo 01"]);
        assert_eq!(menu.labels.status, "Disconnected");
        assert_eq!(menu.labels.connect, "Connect");
        assert_eq!(menu.labels.quit, "Quit");
        handle(TrayEvent::ToggleConnection);
        let menu = super::menu(&ui).unwrap();
        assert!(menu.model.connected);
        assert_eq!(menu.labels.status, "Connected · Tokyo 01");
        assert_eq!(menu.labels.connect, "Disconnect");
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_switch_updates_tray_notice_and_persists_live() {
        let (ui, dir) = setup("desktop-language");
        let actions = ui.global::<Actions>();
        actions.invoke_set_language(1);
        assert_eq!(menu(&ui).unwrap().labels.connect, "Connect");
        actions.invoke_add_node();
        actions.invoke_set_active_node();
        assert_eq!(
            ui.global::<AppState>().get_notice_detail(),
            "Server, UUID, Password"
        );
        actions.invoke_set_language(4);
        assert_eq!(menu(&ui).unwrap().labels.connect, "Подключить");
        assert_eq!(menu(&ui).unwrap().labels.settings, "Настройки…");
        assert_eq!(
            ui.global::<AppState>().get_notice_detail(),
            "Сервер, UUID, Пароль"
        );
        assert_eq!(
            ui.global::<super::super::super::Theme>().get_cjk(),
            "Noto Sans"
        );
        std::thread::sleep(Duration::from_millis(450));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        let saved: crate::config::RuntimeState =
            serde_json::from_slice(&std::fs::read(dir.join("runtime.json")).unwrap()).unwrap();
        assert_eq!(saved.language, crate::config::LanguagePreference::Ru);
        actions.invoke_set_language(1);
        assert_eq!(menu(&ui).unwrap().labels.connect, "Connect");
        assert_eq!(
            ui.global::<AppState>().get_notice_detail(),
            "Server, UUID, Password"
        );
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tray_events_drive_the_ui() {
        let (ui, dir) = setup("desktop-events");
        handle(TrayEvent::SetProxyMode(crate::config::SystemProxyMode::Pac));
        assert_eq!(ui.global::<AppState>().get_proxy_mode(), ProxyMode::Pac);
        handle(TrayEvent::ShowNodes);
        assert_eq!(ui.global::<AppState>().get_page(), Page::Nodes);
        assert!(ui.window().is_visible());
        handle(TrayEvent::ShowLogs);
        assert_eq!(ui.global::<AppState>().get_page(), Page::Logs);
        assert_eq!(logs::refreshing(), Some(Page::Logs));
        handle(TrayEvent::ToggleWindow);
        assert!(!ui.window().is_visible());
        assert_eq!(logs::refreshing(), None);
        handle(TrayEvent::ShowAbout);
        assert_eq!(ui.global::<AppState>().get_page(), Page::Settings);
        assert!(ui.window().is_visible());
        assert!(
            !ui.global::<AppState>().get_reveal_about(),
            "the settings page scrolled to About"
        );
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each prompt trigger crossed with each answer.
    #[test]
    fn unsaved_nodes_prompt_before_close_and_quit() {
        use slint::CloseRequestResponse::{HideWindow, KeepWindowShown};
        i_slint_backend_testing::init_no_event_loop();
        for trigger in ["close", "tray-quit", "app-menu-quit", "hide"] {
            for answer in [Answer::Save, Answer::Discard, Answer::Cancel] {
                let dir = temp_dir(&format!("desktop-prompt-{trigger}-{answer:?}"));
                let ui = MainWindow::new().unwrap();
                install(&ui, controller(&dir).0);
                let actions = ui.global::<Actions>();
                let state = ui.global::<AppState>();
                ui.show().unwrap();
                // Clean: closing proceeds without asking.
                assert!(matches!(close_requested(), HideWindow));
                assert!(!state.get_save_prompt());
                actions.invoke_edit_node(crate::ui::NodeField::Name, "Osaka 01".into());
                match trigger {
                    "close" => assert!(matches!(close_requested(), KeepWindowShown)),
                    "tray-quit" => {
                        // Shown first when the window is hidden.
                        ui.hide().unwrap();
                        handle(TrayEvent::Quit);
                        assert!(ui.window().is_visible());
                    }
                    // The macOS menu item (and Cmd-Q) sends this id
                    // through the tray's menu event handler.
                    "app-menu-quit" => {
                        let id = crate::desktop::tray::APP_MENU_QUIT.menu_id();
                        handle(TrayEvent::from_menu_id(&id).unwrap());
                    }
                    _ => update(|c, _| c.request_leave(Leave::Hide).0),
                }
                assert!(state.get_save_prompt(), "{trigger}");
                // Repeats and a second launch neither answer nor dismiss it.
                assert!(matches!(close_requested(), KeepWindowShown));
                handle(TrayEvent::Quit);
                request_activation();
                assert!(state.get_save_prompt());
                match answer {
                    Answer::Save => actions.invoke_prompt_save(),
                    Answer::Discard => actions.invoke_prompt_discard(),
                    Answer::Cancel => actions.invoke_prompt_cancel(),
                }
                assert!(!state.get_save_prompt());
                let saved = std::fs::read_to_string(dir.join("profiles.json")).unwrap();
                assert_eq!(saved.contains("Osaka 01"), answer == Answer::Save);
                let expected = match answer {
                    Answer::Discard => "Tokyo 01",
                    _ => "Osaka 01",
                };
                assert_eq!(state.get_active_name(), expected);
                // Only an answered hide hides the window; quitting leaves the
                // window to the event loop's end.
                let hidden = trigger == "hide" && answer != Answer::Cancel;
                assert_eq!(ui.window().is_visible(), !hidden, "{trigger} {answer:?}");
                drop(ui);
                shutdown();
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    #[test]
    fn close_quits_unless_it_can_hide_to_the_tray() {
        assert!(!quits_on_close(true, true));
        assert!(quits_on_close(true, false));
        assert!(quits_on_close(false, true));
        assert!(quits_on_close(false, false));
    }

    #[test]
    fn close_to_tray_stops_the_logs_timer_at_once() {
        let (ui, dir) = setup("desktop-close-tray");
        ui.show().unwrap();
        ui.global::<Actions>().invoke_navigate(Page::Logs);
        logs::update_refresh();
        assert_eq!(logs::refreshing(), Some(Page::Logs));
        assert!(matches!(
            close_with_tray(true),
            slint::CloseRequestResponse::HideWindow
        ));
        assert!(!ui.window().is_visible());
        assert_eq!(logs::refreshing(), None, "stopped before the next tick");
        drop(ui);
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_without_tray_hides_and_quits() {
        let (ui, dir) = setup("desktop-close");
        // close_to_tray defaults to on, but no tray icon exists.
        assert!(read(|c| c.close_to_tray()).unwrap());
        assert!(!tray_available());
        assert!(matches!(
            close_requested(),
            slint::CloseRequestResponse::HideWindow
        ));
        drop(ui);
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
