//! Connects the Slint globals to the [`Controller`].
//!
//! Sync discipline: the controller lives in a UI-thread `thread_local!`.
//! Slint getters, setters and callback invocations only happen after its
//! borrow is released, and callbacks that arrive while a snapshot is being
//! pushed (`SYNCING`) or while the controller is borrowed are ignored.
//! Other threads reach the controller through [`post`].

mod desktop;
pub(super) mod logs;
mod nodes;
mod overview;
mod settings;

pub use desktop::{request_activation, show_initial, start_tray, window_ready};

use super::controller::{Changes, Controller, RuleJob};
use super::MainWindow;
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Info notices hide after this long; errors stay until dismissed.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(4);
/// How often the core is checked for an unexpected exit.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static WINDOW: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
    static SYNCING: Cell<bool> = const { Cell::new(false) };
    static LANGUAGE: Cell<Option<crate::i18n::UiLang>> = const { Cell::new(None) };
    static SAVE_TIMER: slint::Timer = slint::Timer::default();
    static NOTICE_TIMER: slint::Timer = slint::Timer::default();
    static POLL_TIMER: slint::Timer = slint::Timer::default();
    static WATCHER: RefCell<Option<notify::RecommendedWatcher>> = const { RefCell::new(None) };
}

/// Coalesces watcher events into one pending reload check.
static RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

/// Install the controller for this thread and wire the UI callbacks.
pub fn install(ui: &MainWindow, controller: Controller) {
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller));
    WINDOW.with(|w| *w.borrow_mut() = Some(ui.as_weak()));
    overview::wire(ui);
    nodes::wire(ui);
    desktop::wire(ui);
    settings::wire(ui);
    logs::wire(ui);
    POLL_TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, POLL_INTERVAL, || {
            // Samples traffic whether or not the Logs page is shown.
            update(|c, _| c.poll_core());
        })
    });
    apply(Changes::OVERVIEW | Changes::EDITOR | Changes::SETTINGS);
}

/// Run startup side effects (PAC server, saved proxy mode, overdue rules).
pub fn startup() {
    if let Some(job) = update_with(|c, now| c.startup(now)).flatten() {
        spawn_rules(job);
    }
}

/// Stop timers and the watcher, then stop the core, restore the system
/// proxy and flush synchronously.
pub fn shutdown() {
    POLL_TIMER.with(|t| t.stop());
    SAVE_TIMER.with(|t| t.stop());
    NOTICE_TIMER.with(|t| t.stop());
    logs::stop();
    WATCHER.with(|w| w.borrow_mut().take());
    desktop::stop();
    LANGUAGE.set(None);
    if let Some(mut controller) = CONTROLLER.with(|c| c.borrow_mut().take()) {
        controller.shutdown();
    }
}

/// Run `f` on the controller and push what it changed into the UI.
pub fn update(f: impl FnOnce(&mut Controller, Instant) -> Changes) {
    update_with(|c, now| (f(c, now), ()));
}

/// Like [`update`], also returning a value. `None` when the call was
/// ignored (re-entrant or during a sync).
pub fn update_with<R>(f: impl FnOnce(&mut Controller, Instant) -> (Changes, R)) -> Option<R> {
    if SYNCING.get() {
        tracing::debug!("ignoring a UI callback during sync");
        return None;
    }
    let result = CONTROLLER.with(|c| {
        let Ok(mut guard) = c.try_borrow_mut() else {
            tracing::warn!("ignoring a re-entrant controller call");
            return None;
        };
        guard
            .as_mut()
            .map(|controller| f(controller, Instant::now()))
    })?;
    let (changes, value) = result;
    apply(changes);
    Some(value)
}

/// Read from the controller without changing it.
fn read<R>(f: impl FnOnce(&Controller) -> R) -> Option<R> {
    CONTROLLER.with(|c| c.try_borrow().ok()?.as_ref().map(f))
}

/// Deliver `f` to the UI thread from any thread.
pub fn post(f: impl FnOnce(&mut Controller, Instant) -> Changes + Send + 'static) {
    if let Err(err) = slint::invoke_from_event_loop(move || update(f)) {
        tracing::warn!("event loop is gone: {err}");
    }
}

/// Push snapshots for `changes` and arm the timers. Never called with the
/// controller borrowed.
fn apply(changes: Changes) {
    let Some(ui) = WINDOW.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade())) else {
        return;
    };
    let language_changed = if changes.settings {
        read(|c| c.language().resolve()).is_some_and(|language| {
            if LANGUAGE.get() == Some(language) {
                return false;
            }
            if let Err(err) = super::apply_language(&ui, language) {
                tracing::warn!("could not switch the language: {err:#}");
            }
            LANGUAGE.set(Some(language));
            true
        })
    } else {
        false
    };
    if changes.overview {
        if let Some(snapshot) = read(|c| c.overview()) {
            sync(|| overview::sync(&ui, &snapshot));
        }
    }
    if changes.nodes || changes.editor || language_changed {
        if let Some(snapshot) = read(|c| c.nodes()) {
            sync(|| nodes::sync(&ui, &snapshot, changes.editor));
        }
    }
    if changes.settings {
        if let Some(snapshot) = read(|c| c.settings()) {
            sync(|| settings::sync(&ui, &snapshot));
        }
    }
    if changes.logs {
        if let Some(snapshot) = read(|c| c.logs()) {
            sync(|| logs::sync_logs(&ui, &snapshot));
        }
    }
    if changes.traffic {
        if let Some(view) = read(|c| c.traffic_view()) {
            sync(|| logs::sync_traffic(&ui, &view));
        }
    }
    if changes.notice {
        if let Some((notice, seq)) = read(|c| c.notice()) {
            sync(|| overview::sync_notice(&ui, &notice));
            NOTICE_TIMER.with(|timer| {
                if notice.is_error() || notice == super::controller::Notice::None {
                    timer.stop();
                } else {
                    timer.start(slint::TimerMode::SingleShot, NOTICE_TIMEOUT, move || {
                        update(|c, _| c.expire_notice(seq));
                    });
                }
            });
        }
    }
    if language_changed && !changes.notice {
        if let Some((notice, _)) = read(|c| c.notice()) {
            sync(|| overview::sync_notice(&ui, &notice));
        }
    }
    if changes.overview || changes.nodes || language_changed {
        desktop::push_tray(&ui, false);
    }
    if changes.persist {
        if let Some(Some(delay)) = read(|c| c.save_delay(Instant::now())) {
            SAVE_TIMER.with(|timer| {
                timer.start(slint::TimerMode::SingleShot, delay, || {
                    update(|c, now| c.flush_due(now));
                })
            });
        }
    }
}

fn sync(f: impl FnOnce()) {
    SYNCING.set(true);
    f();
    SYNCING.set(false);
}

/// Download rules on a worker thread and report back on the UI thread.
pub fn spawn_rules(job: RuleJob) {
    let spawned = std::thread::Builder::new()
        .name("pac-rules".into())
        .spawn(move || {
            let result = job.run();
            post(move |c, _| c.finish_rules(result));
        });
    if let Err(err) = spawned {
        update(|c, _| c.finish_rules(Err(err.into())));
    }
}

/// Files whose external edits are reloaded.
const CONFIG_FILES: [&str; 3] = ["app.json", "profiles.json", "runtime.json"];

/// Whether a watcher event may have changed a config file. Access events
/// are ignored: our own reads in the reload check would otherwise schedule
/// another check forever.
fn is_config_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event.paths.iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| CONFIG_FILES.contains(&name))
    })
}

/// Watch the config directory for external edits, then check once for
/// edits made before the watcher existed.
pub fn watch_config_dir(dir: &std::path::Path) {
    use notify::Watcher;
    let watcher = notify::recommended_watcher(|res: notify::Result<notify::Event>| {
        let relevant = res.as_ref().is_ok_and(is_config_change);
        if relevant && !RELOAD_PENDING.swap(true, Ordering::AcqRel) {
            post(|c, now| {
                RELOAD_PENDING.store(false, Ordering::Release);
                c.on_files_changed(now)
            });
        }
    });
    match watcher {
        Ok(mut watcher) => match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => WATCHER.with(|w| *w.borrow_mut() = Some(watcher)),
            Err(err) => tracing::warn!("config hot-reload disabled for {}: {err}", dir.display()),
        },
        Err(err) => tracing::warn!("config hot-reload disabled: {err}"),
    }
    update(|c, now| c.on_files_changed(now));
}

#[cfg(test)]
mod tests {
    use super::super::controller::testing::{controller, temp_dir, FakeEffects};
    use super::super::{AppState, Connection, NodeStore, Notice, Page};
    use super::*;

    fn setup(name: &str) -> (MainWindow, FakeEffects, std::path::PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, fake) = controller(&dir);
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        (ui, fake, dir)
    }

    fn teardown(dir: &std::path::Path) {
        shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn watcher_reacts_only_to_config_file_changes() {
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind};
        use notify::{Event, EventKind};
        let event = |kind, file: &str| Event::new(kind).add_path(format!("/cfg/{file}").into());
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(is_config_change(&event(modify, "app.json")));
        assert!(is_config_change(&event(
            EventKind::Create(CreateKind::File),
            "runtime.json"
        )));
        assert!(!is_config_change(&event(
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            "profiles.json"
        )));
        assert!(!is_config_change(&event(modify, "china-list.txt")));
    }

    #[test]
    fn callbacks_during_sync_are_ignored() {
        let (ui, fake, dir) = setup("bind-syncing");
        let actions = ui.global::<super::super::Actions>();
        // As if a setter in a snapshot push re-entered a callback.
        sync(|| actions.invoke_toggle_connection());
        assert!(!fake.0.borrow().running);
        assert_eq!(
            ui.global::<AppState>().get_connection(),
            Connection::Stopped
        );
        teardown(&dir);
    }

    #[test]
    fn start_stop_toggles_the_overview() {
        let (ui, fake, dir) = setup("bind-toggle");
        let state = ui.global::<AppState>();
        assert_eq!(state.get_connection(), Connection::Stopped);
        assert_eq!(state.get_active_name(), "Tokyo 01");
        assert_eq!(
            state.get_active_address(),
            "juicity · tokyo.example.com:443"
        );
        ui.global::<super::super::Actions>()
            .invoke_toggle_connection();
        assert_eq!(state.get_connection(), Connection::Running);
        assert!(fake.0.borrow().running);
        ui.global::<super::super::Actions>()
            .invoke_toggle_connection();
        assert_eq!(state.get_connection(), Connection::Stopped);
        assert!(!fake.0.borrow().running);
        teardown(&dir);
    }

    #[test]
    fn background_update_keeps_ui_state_and_does_not_panic() {
        let (ui, fake, dir) = setup("bind-background");
        let actions = ui.global::<super::super::Actions>();
        actions.invoke_toggle_connection();
        // UI-only state the user is working with.
        ui.global::<AppState>().set_page(Page::Overview);
        // Editor text is reloaded only when the draft serial changes.
        let serial = ui.global::<NodeStore>().get_draft_serial();
        fake.0.borrow_mut().exit_reason = Some("Juicity core exited".into());
        // A re-entrant callback while the controller is borrowed is ignored.
        update(|c, _| {
            actions.invoke_toggle_connection();
            c.poll_core()
        });
        let state = ui.global::<AppState>();
        assert_eq!(state.get_connection(), Connection::Stopped);
        assert_eq!(state.get_notice(), Notice::CoreExited);
        assert!(state.get_notice_error());
        assert_eq!(ui.global::<NodeStore>().get_draft_serial(), serial);
        teardown(&dir);
    }

    #[test]
    fn info_banner_hides_and_save_is_debounced() {
        let (ui, fake, dir) = setup("bind-timers");
        let actions = ui.global::<super::super::Actions>();
        actions.invoke_copy_pac_url();
        let state = ui.global::<AppState>();
        assert_eq!(state.get_notice(), Notice::LinkCopied);
        assert_eq!(fake.0.borrow().copied, ["http://127.0.0.1:0/pac"]);
        actions.invoke_set_pac_rule(super::super::PacRule::GfwList);
        assert_eq!(state.get_pac_rule(), super::super::PacRule::GfwList);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(4100));
        assert_eq!(state.get_notice(), Notice::None);
        // The real clock must also pass the debounce before the timer saves.
        std::thread::sleep(DEBOUNCE_WAIT);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        let saved = std::fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(saved.contains("proxy_gfw"), "{saved}");
        teardown(&dir);
    }

    const DEBOUNCE_WAIT: Duration = Duration::from_millis(450);
}
