//! Logs page callbacks, its refresh timer and snapshot sync.

use super::{update, WINDOW};
use crate::traffic::HISTORY;
use crate::ui::controller::{LogsSnapshot, TrafficView};
use crate::ui::{Actions, AppState, LogRow, LogStore, MainWindow, Page, TrafficBar};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::Cell;
use std::time::Duration;

/// How often the shown page reads new log lines and traffic samples.
const REFRESH: Duration = Duration::from_millis(400);

thread_local! {
    static REFRESH_TIMER: slint::Timer = slint::Timer::default();
    /// Log buffer version whose lines are in `LogStore.rows`.
    static SHOWN_VERSION: Cell<Option<u64>> = const { Cell::new(None) };
}

pub fn wire(ui: &MainWindow) {
    let actions = ui.global::<Actions>();
    actions.on_set_log_follow(|follow| update(|c, _| c.set_log_follow(follow)));
    actions.on_clear_logs(|| update(|c, _| c.clear_logs()));
}

fn window() -> Option<MainWindow> {
    WINDOW.with(|w| w.borrow().as_ref().and_then(|w| w.upgrade()))
}

fn page_shown(ui: &MainWindow) -> bool {
    ui.global::<AppState>().get_page() == Page::Logs && ui.window().is_visible()
}

/// Run the refresh timer only while the Logs page is shown in a visible
/// window. Called after navigation and after showing the window; a tick
/// that finds the page or window hidden stops the timer.
pub fn update_refresh() {
    let Some(ui) = window() else {
        return;
    };
    if !page_shown(&ui) {
        stop();
        return;
    }
    if REFRESH_TIMER.with(|timer| timer.running()) {
        return;
    }
    SHOWN_VERSION.set(None);
    update(|c, _| c.reload_logs());
    REFRESH_TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, REFRESH, || {
            if window().is_some_and(|ui| page_shown(&ui)) {
                update(|c, _| c.refresh_logs());
            } else {
                stop();
            }
        })
    });
}

pub fn stop() {
    REFRESH_TIMER.with(|timer| timer.stop());
}

#[cfg(test)]
pub fn refreshing() -> bool {
    REFRESH_TIMER.with(|timer| timer.running())
}

pub fn sync_logs(ui: &MainWindow, snapshot: &LogsSnapshot) {
    let store = ui.global::<LogStore>();
    if SHOWN_VERSION.get() != Some(snapshot.version) {
        SHOWN_VERSION.set(Some(snapshot.version));
        let max_chars = snapshot
            .lines
            .iter()
            .map(|line| line.target.chars().count() + line.message.chars().count())
            .max()
            .unwrap_or(0);
        let rows: Vec<LogRow> = snapshot
            .lines
            .iter()
            .map(|line| LogRow {
                time: line.time.as_str().into(),
                level: line.level.as_str().into(),
                target: line.target.as_str().into(),
                message: line.message.as_str().into(),
            })
            .collect();
        store.set_max_chars(i32::try_from(max_chars).unwrap_or(i32::MAX));
        store.set_rows(ModelRc::new(VecModel::from(rows)));
        store.set_serial(store.get_serial().wrapping_add(1));
    }
    store.set_dropped(i32::try_from(snapshot.dropped).unwrap_or(i32::MAX));
    store.set_follow(snapshot.follow);
}

pub fn sync_traffic(ui: &MainWindow, view: &TrafficView) {
    let store = ui.global::<LogStore>();
    let bars: Vec<TrafficBar> = view
        .bars
        .iter()
        .map(|bar| TrafficBar {
            slot: bar.slot as i32,
            down: bar.down,
            up: bar.up,
        })
        .collect();
    store.set_slots(HISTORY as i32);
    store.set_bars(ModelRc::new(VecModel::from(bars)));
    store.set_has_traffic(view.has_samples);
    store.set_down_speed(view.down_speed.as_str().into());
    store.set_up_speed(view.up_speed.as_str().into());
    store.set_down_total(view.down_total.as_str().into());
    store.set_up_total(view.up_total.as_str().into());
}

/// Push fixed page data without a controller (screenshots).
#[cfg(test)]
pub fn sync_fixture(ui: &MainWindow, snapshot: &LogsSnapshot, view: &TrafficView) {
    SHOWN_VERSION.set(None);
    sync_logs(ui, snapshot);
    sync_traffic(ui, view);
}

#[cfg(test)]
mod tests {
    use super::super::super::controller::testing::{controller, temp_dir};
    use super::super::{install, shutdown};
    use super::*;
    use crate::logging::LogBuffer;
    use crate::traffic::TrafficMonitor;
    use slint::Model;
    use std::sync::Arc;

    fn setup(name: &str) -> (MainWindow, Arc<LogBuffer>, std::path::PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, _) = controller(&dir);
        let buffer = Arc::new(LogBuffer::new());
        let c = c.with_log_sources(Arc::clone(&buffer), Arc::new(TrafficMonitor::new()));
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        (ui, buffer, dir)
    }

    #[test]
    fn refresh_runs_only_while_the_page_and_window_are_shown() {
        let (ui, buffer, dir) = setup("logs-timer");
        let actions = ui.global::<Actions>();
        // Hidden window: the page alone does not start the timer.
        actions.invoke_navigate(Page::Logs);
        assert!(!refreshing());
        ui.show().unwrap();
        update_refresh();
        assert!(refreshing());

        buffer.push(tracing::Level::WARN, "juicity_gui::core", "boom".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        let store = ui.global::<LogStore>();
        assert_eq!(store.get_rows().row_count(), 1);
        let row = store.get_rows().row_data(0).unwrap();
        assert_eq!(
            (row.level.as_str(), row.target.as_str()),
            ("WARN", "juicity_gui::core")
        );

        actions.invoke_navigate(Page::Overview);
        assert!(!refreshing(), "another page stops the timer");
        actions.invoke_navigate(Page::Logs);
        assert!(refreshing());
        ui.hide().unwrap();
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert!(!refreshing(), "a hidden window stops the timer");
        // Lines that arrive while hidden are not pushed.
        buffer.push(tracing::Level::INFO, "juicity_gui::core", "late".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(store.get_rows().row_count(), 1);
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_user_scroll_up_turns_follow_off() {
        use slint::platform::WindowEvent;
        use slint::LogicalPosition;
        let (ui, buffer, dir) = setup("logs-follow");
        ui.window().set_size(slint::LogicalSize::new(960.0, 640.0));
        ui.show().unwrap();
        ui.global::<Actions>().invoke_navigate(Page::Logs);
        let store = ui.global::<LogStore>();
        let push = |count: usize| {
            for i in 0..count {
                buffer.push(
                    tracing::Level::INFO,
                    "juicity_gui::core",
                    format!("line {i}"),
                );
            }
            i_slint_backend_testing::mock_elapsed_time(REFRESH);
        };
        push(200);
        assert_eq!(store.get_rows().row_count(), 200);
        assert!(store.get_follow());
        // New lines move the list to the tail; that jump keeps Follow on.
        push(20);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        assert!(store.get_follow(), "a programmatic tail jump keeps Follow");
        // A wheel tick down at the tail keeps it too.
        let over_list = LogicalPosition::new(600.0, 560.0);
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: over_list,
            delta_x: 0.0,
            delta_y: -40.0,
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
        assert!(store.get_follow(), "scrolling down keeps Follow");
        // One wheel tick up turns it off before the scroll animation moves.
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: over_list,
            delta_x: 0.0,
            delta_y: 40.0,
        });
        assert!(!store.get_follow(), "a wheel tick up turns Follow off");
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn follow_and_clear_reach_the_page() {
        let (ui, buffer, dir) = setup("logs-actions");
        ui.show().unwrap();
        ui.global::<Actions>().invoke_navigate(Page::Logs);
        buffer.push(tracing::Level::INFO, "juicity_gui::core", "one".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        let store = ui.global::<LogStore>();
        assert_eq!(store.get_rows().row_count(), 1);
        let actions = ui.global::<Actions>();
        actions.invoke_set_log_follow(false);
        assert!(!store.get_follow());
        actions.invoke_clear_logs();
        assert_eq!(store.get_rows().row_count(), 0);
        assert_eq!(store.get_dropped(), 0);
        assert!(!store.get_follow());
        shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
