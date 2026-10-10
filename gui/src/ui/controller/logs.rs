//! Logs page: captured log lines, the traffic chart and its statistics.

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
    /// Buffer and monitor versions last reported by `refresh_logs`.
    seen: (u64, u64),
}

impl LogsState {
    /// The process-wide log buffer and traffic monitor.
    pub(super) fn new() -> Self {
        Self::with_sources(crate::logging::buffer(), traffic::monitor())
    }

    pub(super) fn with_sources(buffer: Arc<LogBuffer>, traffic: Arc<TrafficMonitor>) -> Self {
        let seen = (buffer.version(), traffic.version());
        Self {
            buffer,
            traffic,
            follow: true,
            seen,
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

/// One chart slot; heights are fractions of half the chart height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChartBar {
    /// 0 is the oldest of `traffic::HISTORY` slots.
    pub slot: usize,
    pub down: f32,
    pub up: f32,
}

/// The traffic chart and statistics as the page shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct TrafficView {
    pub bars: Vec<ChartBar>,
    pub has_samples: bool,
    pub down_speed: String,
    pub up_speed: String,
    pub down_total: String,
    pub up_total: String,
}

/// Map samples to bars: right-aligned in `HISTORY` slots, scaled to the
/// larger peak but never below 1 KiB/s.
fn chart_bars(snapshot: &TrafficSnapshot) -> Vec<ChartBar> {
    let scale = snapshot.peak_down.max(snapshot.peak_up).max(SCALE_FLOOR);
    let samples = &snapshot.samples[snapshot.samples.len().saturating_sub(traffic::HISTORY)..];
    let offset = traffic::HISTORY - samples.len();
    samples
        .iter()
        .enumerate()
        .map(|(index, sample)| ChartBar {
            slot: offset + index,
            down: (sample.down / scale).clamp(0.0, 1.0) as f32,
            up: (sample.up / scale).clamp(0.0, 1.0) as f32,
        })
        .collect()
}

pub fn traffic_view(snapshot: &TrafficSnapshot) -> TrafficView {
    TrafficView {
        bars: chart_bars(snapshot),
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

    /// Refresh timer of the shown page: report what changed since the last call.
    pub fn refresh_logs(&mut self) -> Changes {
        let seen = (self.logs.buffer.version(), self.logs.traffic.version());
        let changes = Changes {
            logs: seen.0 != self.logs.seen.0,
            traffic: seen.1 != self.logs.seen.1,
            ..Changes::NONE
        };
        self.logs.seen = seen;
        changes
    }

    /// The page became visible: push everything.
    pub fn reload_logs(&mut self) -> Changes {
        self.logs.seen = (self.logs.buffer.version(), self.logs.traffic.version());
        Changes::LOGS | Changes::TRAFFIC
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
        self.logs.seen.0 = self.logs.buffer.version();
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
        assert_eq!(c.refresh_logs(), Changes::TRAFFIC);
        assert_eq!(c.refresh_logs(), Changes::NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chart_bars_are_right_aligned_with_a_1_kib_floor() {
        let view = traffic_view(&speeds(&[(512.0, 0.0), (256.0, 1024.0)]));
        assert_eq!(
            view.bars,
            [
                ChartBar {
                    slot: traffic::HISTORY - 2,
                    down: 0.5,
                    up: 0.0
                },
                ChartBar {
                    slot: traffic::HISTORY - 1,
                    down: 0.25,
                    up: 1.0
                },
            ]
        );
        assert!(view.has_samples);
        // Above the floor, the larger peak of either direction is the scale.
        let bars = chart_bars(&speeds(&[(8192.0, 2048.0), (4096.0, 0.0)]));
        assert_eq!(bars[0].down, 1.0);
        assert_eq!(bars[0].up, 0.25);
        assert_eq!(bars[1].down, 0.5);
        // A full history fills every slot from 0.
        let full = vec![(10.0, 10.0); traffic::HISTORY];
        let bars = chart_bars(&speeds(&full));
        assert_eq!(bars.first().unwrap().slot, 0);
        assert_eq!(bars.last().unwrap().slot, traffic::HISTORY - 1);
        let empty = traffic_view(&TrafficSnapshot::default());
        assert!(empty.bars.is_empty() && !empty.has_samples);
        assert_eq!(empty.down_speed, traffic::format_speed(0.0));
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
