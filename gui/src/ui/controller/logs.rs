//! Captured log lines (Logs page), the traffic chart and its statistics
//! (Overview page).

use super::{Changes, Controller};
use crate::logging::{LogBuffer, LogLine};
use crate::traffic::{self, TrafficMonitor, TrafficSnapshot};
use std::sync::Arc;

/// Lowest chart scale in bytes per second, so idle traffic stays small.
const SCALE_FLOOR: f64 = 1024.0;

/// Where the page reads from, plus its own state.
pub(super) struct LogsState {
    pub(super) buffer: Arc<LogBuffer>,
    pub(super) traffic: Arc<TrafficMonitor>,
    follow: bool,
    /// Buffer version last reported by `refresh_logs`.
    seen_logs: u64,
    /// Monitor version last reported by `refresh_traffic`.
    seen_traffic: u64,
}

impl LogsState {
    /// The process-wide log buffer and traffic monitor.
    pub(super) fn new() -> Self {
        Self::with_sources(crate::logging::buffer(), traffic::monitor())
    }

    pub(super) fn with_sources(buffer: Arc<LogBuffer>, traffic: Arc<TrafficMonitor>) -> Self {
        Self {
            seen_logs: buffer.version(),
            seen_traffic: traffic.version(),
            buffer,
            traffic,
            follow: true,
        }
    }
}

/// The log list as the page shows it.
#[derive(Clone, Debug)]
pub struct LogsSnapshot {
    /// Log buffer version of `lines`.
    pub version: u64,
    pub lines: Vec<LogLine>,
    pub dropped: u64,
    pub follow: bool,
}

/// One cubic Bézier segment: start, two control points, end. `x` counts
/// `traffic::HISTORY` slots from 0 (oldest); `y` is a fraction of the scale,
/// 0 at the baseline.
type Segment = [(f64, f64); 4];

/// The chart curves as Slint path commands in a viewbox `HISTORY - 1` slots
/// wide and 1 high, so they do not depend on the chart's pixel size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChartPaths {
    pub down_line: String,
    /// `down_line` closed along the baseline, for the area fill.
    pub down_area: String,
    pub up_line: String,
}

/// The traffic chart and statistics as the Overview page shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct TrafficView {
    pub chart: ChartPaths,
    pub has_samples: bool,
    pub down_speed: String,
    pub up_speed: String,
    pub down_total: String,
    pub up_total: String,
}

/// Monotone cubic interpolation (Fritsch–Butland) through evenly spaced
/// values starting at slot `first`. Each tangent is the harmonic mean of the
/// neighbouring slopes, or 0 at a local extreme, which keeps every control
/// point between the two samples it joins: the curve never overshoots.
fn monotone_segments(first: usize, values: &[f64]) -> Vec<Segment> {
    let slopes: Vec<f64> = values.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let tangent = |index: usize| -> f64 {
        match (index.checked_sub(1).map(|i| slopes[i]), slopes.get(index)) {
            (Some(before), Some(&after)) if before * after > 0.0 => {
                2.0 * before * after / (before + after)
            }
            (Some(_), Some(_)) => 0.0,
            (None, Some(&only)) | (Some(only), None) => only,
            (None, None) => 0.0,
        }
    };
    (0..slopes.len())
        .map(|index| {
            let x = (first + index) as f64;
            let (start, end) = (values[index], values[index + 1]);
            [
                (x, start),
                (x + 1.0 / 3.0, start + tangent(index) / 3.0),
                (x + 2.0 / 3.0, end - tangent(index + 1) / 3.0),
                (x + 1.0, end),
            ]
        })
        .collect()
}

/// Download and upload curves: right-aligned in `HISTORY` slots, both scaled
/// to the larger peak but never below 1 KiB/s.
fn chart_curves(snapshot: &TrafficSnapshot) -> (Vec<Segment>, Vec<Segment>) {
    let scale = snapshot.peak_down.max(snapshot.peak_up).max(SCALE_FLOOR);
    let samples = &snapshot.samples[snapshot.samples.len().saturating_sub(traffic::HISTORY)..];
    let first = traffic::HISTORY - samples.len();
    let series = |pick: fn(&traffic::Speed) -> f64| -> Vec<f64> {
        samples
            .iter()
            .map(|sample| (pick(sample) / scale).clamp(0.0, 1.0))
            .collect()
    };
    (
        monotone_segments(first, &series(|s| s.down)),
        monotone_segments(first, &series(|s| s.up)),
    )
}

/// One viewbox point; the viewbox `y` grows downwards from the top.
fn point((x, y): (f64, f64)) -> String {
    format!("{x:.3} {:.4}", 1.0 - y)
}

/// Path commands of a curve; empty with fewer than two samples.
fn line_commands(segments: &[Segment]) -> String {
    let Some(first) = segments.first() else {
        return String::new();
    };
    let mut commands = format!("M {}", point(first[0]));
    for [_, c1, c2, end] in segments {
        commands += &format!(" C {} {} {}", point(*c1), point(*c2), point(*end));
    }
    commands
}

/// `line_commands` closed along the baseline.
fn area_commands(segments: &[Segment]) -> String {
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        return String::new();
    };
    format!(
        "{} L {} L {} Z",
        line_commands(segments),
        point((last[3].0, 0.0)),
        point((first[0].0, 0.0))
    )
}

fn chart_paths(snapshot: &TrafficSnapshot) -> ChartPaths {
    let (down, up) = chart_curves(snapshot);
    ChartPaths {
        down_line: line_commands(&down),
        down_area: area_commands(&down),
        up_line: line_commands(&up),
    }
}

pub fn traffic_view(snapshot: &TrafficSnapshot) -> TrafficView {
    TrafficView {
        chart: chart_paths(snapshot),
        has_samples: !snapshot.samples.is_empty(),
        down_speed: traffic::format_speed(snapshot.current.down),
        up_speed: traffic::format_speed(snapshot.current.up),
        down_total: traffic::format_bytes(snapshot.total_down),
        up_total: traffic::format_bytes(snapshot.total_up),
    }
}

impl Controller {
    pub fn logs(&self) -> LogsSnapshot {
        let buffer = &self.logs.buffer;
        LogsSnapshot {
            version: buffer.version(),
            lines: buffer.snapshot(),
            dropped: buffer.dropped(),
            follow: self.logs.follow,
        }
    }

    pub fn traffic_view(&self) -> TrafficView {
        traffic_view(&self.logs.traffic.snapshot())
    }

    /// Refresh timer of the Logs page: report new lines since the last call.
    pub fn refresh_logs(&mut self) -> Changes {
        let seen = self.logs.buffer.version();
        let changed = seen != self.logs.seen_logs;
        self.logs.seen_logs = seen;
        if changed {
            Changes::LOGS
        } else {
            Changes::NONE
        }
    }

    /// Refresh timer of the Overview page: report new traffic samples since
    /// the last call.
    pub fn refresh_traffic(&mut self) -> Changes {
        let seen = self.logs.traffic.version();
        let changed = seen != self.logs.seen_traffic;
        self.logs.seen_traffic = seen;
        if changed {
            Changes::TRAFFIC
        } else {
            Changes::NONE
        }
    }

    /// The Logs page became visible: push all lines.
    pub fn reload_logs(&mut self) -> Changes {
        self.logs.seen_logs = self.logs.buffer.version();
        Changes::LOGS
    }

    /// The Overview page became visible: push the whole chart.
    pub fn reload_traffic(&mut self) -> Changes {
        self.logs.seen_traffic = self.logs.traffic.version();
        Changes::TRAFFIC
    }

    pub fn set_log_follow(&mut self, follow: bool) -> Changes {
        if self.logs.follow == follow {
            return Changes::NONE;
        }
        self.logs.follow = follow;
        Changes::LOGS
    }

    /// Empty the buffer and reset the dropped count.
    pub fn clear_logs(&mut self) -> Changes {
        self.logs.buffer.clear();
        self.logs.seen_logs = self.logs.buffer.version();
        Changes::LOGS
    }

    /// Feed the core's counters to the traffic monitor (every poll tick).
    pub(super) fn sample_traffic(&mut self) {
        let counters = self.effects.traffic(&mut self.gui.core_manager);
        self.logs.traffic.record(counters);
    }

    /// Use private log and traffic sources instead of the process-wide ones.
    #[cfg(test)]
    pub fn with_log_sources(
        mut self,
        buffer: Arc<LogBuffer>,
        traffic: Arc<TrafficMonitor>,
    ) -> Self {
        self.logs = LogsState::with_sources(buffer, traffic);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::traffic::Speed;
    use tracing::Level;

    fn sources() -> (Arc<LogBuffer>, Arc<TrafficMonitor>) {
        (Arc::new(LogBuffer::new()), Arc::new(TrafficMonitor::new()))
    }

    fn speeds(values: &[(f64, f64)]) -> TrafficSnapshot {
        let samples: Vec<Speed> = values
            .iter()
            .map(|&(down, up)| Speed { up, down })
            .collect();
        TrafficSnapshot {
            peak_down: samples.iter().map(|s| s.down).fold(0.0, f64::max),
            peak_up: samples.iter().map(|s| s.up).fold(0.0, f64::max),
            current: samples.last().copied().unwrap_or_default(),
            samples,
            ..TrafficSnapshot::default()
        }
    }

    #[test]
    fn poll_records_the_fake_counters_every_tick() {
        let dir = temp_dir("logs-poll");
        let (c, fake) = controller(&dir);
        let (buffer, monitor) = sources();
        let mut c = c.with_log_sources(buffer, Arc::clone(&monitor));
        // Sampled while stopped too: the page or window state plays no part.
        fake.0.borrow_mut().traffic = Some((100, 200));
        let _ = c.poll_core();
        let _ = c.poll_core();
        assert_eq!(fake.0.borrow().traffic_reads, 2);
        let before = monitor.version();
        // A core that stops reports no counters: the speed decays to a zero sample.
        fake.0.borrow_mut().traffic = None;
        let _ = c.poll_core();
        assert_eq!(fake.0.borrow().traffic_reads, 3);
        assert!(monitor.version() > before);
        assert_eq!(monitor.snapshot().samples, [Speed::default()]);
        // Traffic reaches the Overview refresh only, never the Logs one.
        assert_eq!(c.refresh_logs(), Changes::NONE);
        assert_eq!(c.refresh_traffic(), Changes::TRAFFIC);
        assert_eq!(c.refresh_traffic(), Changes::NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every control point lies between the two samples its segment joins.
    fn assert_monotone(segments: &[Segment]) {
        for [start, c1, c2, end] in segments {
            let (low, high) = (start.1.min(end.1), start.1.max(end.1));
            for (_, y) in [c1, c2] {
                assert!(
                    (low - 1e-12..=high + 1e-12).contains(y),
                    "{y} outside {low}..={high}"
                );
            }
        }
    }

    #[test]
    fn chart_paths_need_two_samples() {
        let empty = traffic_view(&TrafficSnapshot::default());
        assert_eq!(empty.chart, ChartPaths::default());
        assert!(!empty.has_samples);
        assert_eq!(empty.down_speed, traffic::format_speed(0.0));
        let one = traffic_view(&speeds(&[(512.0, 256.0)]));
        assert_eq!(one.chart, ChartPaths::default());
        assert!(one.has_samples);
    }

    #[test]
    fn chart_curves_are_right_aligned_with_a_1_kib_floor() {
        let (down, up) = chart_curves(&speeds(&[(512.0, 0.0), (256.0, 1024.0)]));
        let last = (traffic::HISTORY - 1) as f64;
        assert_eq!(down.len(), 1);
        assert_eq!((down[0][0], down[0][3]), ((last - 1.0, 0.5), (last, 0.25)));
        assert_eq!((up[0][0], up[0][3]), ((last - 1.0, 0.0), (last, 1.0)));
        // Above the floor, the larger peak of either direction is the scale.
        let (down, up) = chart_curves(&speeds(&[(8192.0, 2048.0), (4096.0, 0.0)]));
        assert_eq!((down[0][0].1, down[0][3].1), (1.0, 0.5));
        assert_eq!(up[0][0].1, 0.25);
        // A full history spans every slot from 0.
        let full = vec![(10.0, 10.0); traffic::HISTORY + 5];
        let (down, _) = chart_curves(&speeds(&full));
        assert_eq!(down.len(), traffic::HISTORY - 1);
        assert_eq!(down.first().unwrap()[0].0, 0.0);
        assert_eq!(down.last().unwrap()[3].0, last);
    }

    #[test]
    fn chart_curves_never_overshoot() {
        let values = [
            (0.0, 9000.0),
            (8000.0, 0.0),
            (8000.0, 100.0),
            (100.0, 5000.0),
            (7000.0, 5100.0),
            (0.0, 0.0),
            (300.0, 9000.0),
        ];
        let (down, up) = chart_curves(&speeds(&values));
        assert_monotone(&down);
        assert_monotone(&up);
        // A plateau and a peak stay flat: no bulge past either sample.
        assert_eq!(down[1][1].1, down[1][0].1);
        assert_eq!(down[1][2].1, down[1][3].1);
    }

    #[test]
    fn chart_paths_use_viewbox_coordinates() {
        let view = traffic_view(&speeds(&[(0.0, 1024.0), (1024.0, 1024.0)]));
        let (a, b) = (traffic::HISTORY - 2, traffic::HISTORY - 1);
        // The viewbox y grows downwards: full scale is 0, the baseline 1.
        assert_eq!(
            view.chart.down_line,
            format!("M {a}.000 1.0000 C {a}.333 0.6667 {a}.667 0.3333 {b}.000 0.0000")
        );
        assert_eq!(
            view.chart.down_area,
            format!(
                "{} L {b}.000 1.0000 L {a}.000 1.0000 Z",
                view.chart.down_line
            )
        );
        assert!(view
            .chart
            .up_line
            .starts_with(&format!("M {a}.000 0.0000 C")));
    }

    #[test]
    fn follow_clear_and_dropped_lines() {
        let dir = temp_dir("logs-follow");
        let (c, _) = controller(&dir);
        let (buffer, monitor) = sources();
        let mut c = c.with_log_sources(Arc::clone(&buffer), monitor);
        assert!(c.logs().follow);
        assert_eq!(c.set_log_follow(false), Changes::LOGS);
        assert!(!c.logs().follow);
        assert_eq!(c.set_log_follow(false), Changes::NONE);
        assert_eq!(c.set_log_follow(true), Changes::LOGS);

        for index in 0..2005 {
            buffer.push(Level::INFO, "juicity_gui::core", format!("line {index}"));
        }
        assert_eq!(c.refresh_traffic(), Changes::NONE);
        assert_eq!(c.refresh_logs(), Changes::LOGS);
        let logs = c.logs();
        assert_eq!(logs.lines.len(), 2000);
        assert_eq!(logs.dropped, 5);
        assert_eq!(logs.lines[0].message, "line 5");
        assert_eq!(logs.lines[0].target, "juicity_gui::core");

        assert_eq!(c.clear_logs(), Changes::LOGS);
        let logs = c.logs();
        assert!(logs.lines.is_empty());
        assert_eq!(logs.dropped, 0);
        assert!(logs.follow, "Clear keeps Follow");
        // The clear was already reported.
        assert_eq!(c.refresh_logs(), Changes::NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
