//! Logs page callbacks, the page refresh timer (log lines on Logs, traffic
//! on Overview) and snapshot sync.

use super::{update, WINDOW};
use crate::traffic::HISTORY;
use crate::ui::controller::{LogsSnapshot, TrafficView};
use crate::ui::{Actions, AppState, LogRow, LogStore, MainWindow, Page, TrafficBar, TrafficStore};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::Cell;
use std::time::Duration;

/// How often the shown page reads new log lines (Logs) or traffic samples
/// (Overview).
const REFRESH: Duration = Duration::from_millis(400);

thread_local! {
    static REFRESH_TIMER: slint::Timer = slint::Timer::default();
    /// Page whose data the refresh timer pushes.
    static REFRESH_PAGE: Cell<Option<Page>> = const { Cell::new(None) };
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

/// The shown page that needs refreshing: Overview or Logs in a visible
/// window.
fn refreshed_page(ui: &MainWindow) -> Option<Page> {
    let page = ui.global::<AppState>().get_page();
    (matches!(page, Page::Overview | Page::Logs) && ui.window().is_visible()).then_some(page)
}

/// Run the refresh timer only while Overview (traffic) or Logs (log lines)
/// is shown in a visible window. Called after navigation and after showing
/// the window; a tick that finds its page or the window hidden stops the
/// timer.
pub fn update_refresh() {
    let Some(ui) = window() else {
        return;
    };
    let Some(page) = refreshed_page(&ui) else {
        stop();
        return;
    };
    if refreshing() == Some(page) {
        return;
    }
    REFRESH_PAGE.set(Some(page));
    if page == Page::Logs {
        SHOWN_VERSION.set(None);
        update(|c, _| c.reload_logs());
    } else {
        update(|c, _| c.reload_traffic());
    }
    REFRESH_TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, REFRESH, move || {
            if window().and_then(|ui| refreshed_page(&ui)) != Some(page) {
                stop();
            } else if page == Page::Logs {
                update(|c, _| c.refresh_logs());
            } else {
                update(|c, _| c.refresh_traffic());
            }
        })
    });
}

pub fn stop() {
    REFRESH_TIMER.with(|timer| timer.stop());
}

/// The page the refresh timer is running for.
pub fn refreshing() -> Option<Page> {
    REFRESH_TIMER
        .with(|timer| timer.running())
        .then(|| REFRESH_PAGE.get())
        .flatten()
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
    let store = ui.global::<TrafficStore>();
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

    fn setup_with_monitor(
        name: &str,
    ) -> (
        MainWindow,
        Arc<LogBuffer>,
        Arc<TrafficMonitor>,
        std::path::PathBuf,
    ) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = temp_dir(name);
        let (c, _) = controller(&dir);
        let buffer = Arc::new(LogBuffer::new());
        let monitor = Arc::new(TrafficMonitor::new());
        let c = c.with_log_sources(Arc::clone(&buffer), Arc::clone(&monitor));
        let ui = MainWindow::new().unwrap();
        install(&ui, c);
        (ui, buffer, monitor, dir)
    }

    fn setup(name: &str) -> (MainWindow, Arc<LogBuffer>, std::path::PathBuf) {
        let (ui, buffer, _, dir) = setup_with_monitor(name);
        (ui, buffer, dir)
    }

    /// Record one (zero) traffic sample: a running core, then a stopped one.
    fn sample(monitor: &TrafficMonitor) {
        monitor.record(Some((0, 0)));
        monitor.record(None);
    }

    #[test]
    fn traffic_refreshes_on_overview_and_lines_on_logs() {
        let (ui, buffer, monitor, dir) = setup_with_monitor("logs-timer");
        let actions = ui.global::<Actions>();
        let rows = || ui.global::<LogStore>().get_rows().row_count();
        let bars = || ui.global::<TrafficStore>().get_bars().row_count();
        // Hidden window: the page alone does not start the timer.
        actions.invoke_navigate(Page::Overview);
        assert_eq!(refreshing(), None);
        ui.show().unwrap();
        update_refresh();
        assert_eq!(refreshing(), Some(Page::Overview));
        assert!(!ui.global::<TrafficStore>().get_has_traffic());

        // Overview pushes traffic but not log lines.
        sample(&monitor);
        buffer.push(tracing::Level::WARN, "juicity_gui::core", "boom".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(bars(), 1);
        assert!(ui.global::<TrafficStore>().get_has_traffic());
        assert_eq!(rows(), 0, "log lines do not refresh on Overview");

        // Logs pushes log lines but not traffic.
        actions.invoke_navigate(Page::Logs);
        assert_eq!(refreshing(), Some(Page::Logs));
        assert_eq!(rows(), 1);
        let row = ui.global::<LogStore>().get_rows().row_data(0).unwrap();
        assert_eq!(
            (row.level.as_str(), row.target.as_str()),
            ("WARN", "juicity_gui::core")
        );
        sample(&monitor);
        buffer.push(tracing::Level::INFO, "juicity_gui::core", "two".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(rows(), 2);
        assert_eq!(bars(), 1, "traffic does not refresh on Logs");

        // Back on Overview, the chart catches up at once.
        actions.invoke_navigate(Page::Overview);
        assert_eq!(refreshing(), Some(Page::Overview));
        assert_eq!(bars(), 2);
        actions.invoke_navigate(Page::Nodes);
        assert_eq!(refreshing(), None, "another page stops the timer");

        // A hidden window stops either refresh, and nothing is pushed.
        actions.invoke_navigate(Page::Overview);
        ui.hide().unwrap();
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(
            refreshing(),
            None,
            "a hidden window stops the traffic timer"
        );
        sample(&monitor);
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(bars(), 2);
        ui.show().unwrap();
        actions.invoke_navigate(Page::Logs);
        assert_eq!(refreshing(), Some(Page::Logs));
        ui.hide().unwrap();
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(refreshing(), None, "a hidden window stops the logs timer");
        buffer.push(tracing::Level::INFO, "juicity_gui::core", "late".into());
        i_slint_backend_testing::mock_elapsed_time(REFRESH);
        assert_eq!(rows(), 2);
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
