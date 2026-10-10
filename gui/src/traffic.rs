//! Traffic sampling for the log window's chart and statistics.
//!
//! The proxy core reports *cumulative* byte counters.  This module samples them
//! on a timer and turns them into per-second speeds plus running totals.
//!
//! Counters restart whenever a connection is replaced, so a decrease is not a
//! negative delta: it is treated as a fresh baseline and only the bytes since
//! then are counted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Number of one-second speed samples retained — the chart window.
pub const HISTORY: usize = 60;

/// Minimum interval between two recorded samples.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(1000);

/// A chart point: upload and download speed in bytes per second.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Speed {
    pub up: f64,
    pub down: f64,
}

/// Read-only view handed to the UI.
#[derive(Clone, Debug, Default)]
pub struct TrafficSnapshot {
    /// Speeds, oldest first (at most [`HISTORY`] entries).
    pub samples: Vec<Speed>,
    /// Total bytes since the application started.
    pub total_up: u64,
    pub total_down: u64,
    /// Most recent speed.
    pub current: Speed,
    /// Largest speed over the retained window.
    pub peak_up: f64,
    pub peak_down: f64,
}

struct Data {
    samples: VecDeque<Speed>,
    total_up: u64,
    total_down: u64,
    /// Last cumulative reading, used to compute deltas.
    previous: Option<(u64, u64)>,
    /// Bytes accumulated since the last sample was flushed.
    pending: (u64, u64),
    /// When the last sample was flushed.
    last_sample: Option<Instant>,
    current: Speed,
    active: bool,
}

/// Samples cumulative byte counters into speeds and running totals.
pub struct TrafficMonitor {
    data: Mutex<Data>,
    version: AtomicU64,
}

impl TrafficMonitor {
    pub(crate) fn new() -> Self {
        Self {
            data: Mutex::new(Data {
                samples: VecDeque::new(),
                total_up: 0,
                total_down: 0,
                previous: None,
                pending: (0, 0),
                last_sample: None,
                current: Speed::default(),
                active: false,
            }),
            version: AtomicU64::new(0),
        }
    }

    /// Feed the latest cumulative `(transmitted, received)` byte counts.
    ///
    /// `None` means no core is running: the baseline is dropped so the next
    /// core does not start with a bogus spike, and the speed decays to zero.
    pub fn record(&self, cumulative: Option<(u64, u64)>) {
        self.record_at(cumulative, Instant::now());
    }

    fn record_at(&self, cumulative: Option<(u64, u64)>, now: Instant) {
        let mut data = self.lock();
        let mut changed = false;

        match cumulative {
            None => {
                if data.previous.is_some() || data.current != Speed::default() || data.active {
                    data.current = Speed::default();
                    data.active = false;
                    push_sample(&mut data, Speed::default());
                    changed = true;
                }
                data.previous = None;
                data.pending = (0, 0);
                data.last_sample = None;
            }
            Some((up, down)) => {
                if let Some((prev_up, prev_down)) = data.previous {
                    data.pending.0 += up.saturating_sub(prev_up);
                    data.pending.1 += down.saturating_sub(prev_down);
                }
                data.previous = Some((up, down));
                data.active = true;

                let since = match data.last_sample {
                    Some(last) => now.duration_since(last),
                    None => {
                        // First reading after a (re)start: only establishes the
                        // baseline, so the initial delta is not attributed.
                        data.last_sample = Some(now);
                        Duration::ZERO
                    }
                };

                if since >= SAMPLE_INTERVAL {
                    let secs = since.as_secs_f64().max(0.001);
                    let speed = Speed {
                        up: data.pending.0 as f64 / secs,
                        down: data.pending.1 as f64 / secs,
                    };
                    data.total_up = data.total_up.saturating_add(data.pending.0);
                    data.total_down = data.total_down.saturating_add(data.pending.1);
                    data.pending = (0, 0);
                    data.current = speed;
                    data.last_sample = Some(now);
                    push_sample(&mut data, speed);
                    changed = true;
                }
            }
        }

        drop(data);
        if changed {
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// Drop the delta baseline, e.g. when the core is replaced.
    ///
    /// Totals and the recorded history are kept, but the next reading starts a
    /// fresh baseline — otherwise the new core's counters (which restart at
    /// zero) would look like a huge decrease and hide all traffic.
    pub fn reset(&self) {
        let mut data = self.lock();
        data.previous = None;
        data.pending = (0, 0);
        data.last_sample = None;
        data.current = Speed::default();
        data.active = false;
    }

    /// Snapshot of everything the UI needs to draw.
    pub fn snapshot(&self) -> TrafficSnapshot {
        let data = self.lock();
        let samples: Vec<Speed> = data.samples.iter().copied().collect();
        TrafficSnapshot {
            peak_up: samples.iter().map(|s| s.up).fold(0.0, f64::max),
            peak_down: samples.iter().map(|s| s.down).fold(0.0, f64::max),
            samples,
            total_up: data.total_up,
            total_down: data.total_down,
            current: data.current,
        }
    }

    /// Monotonic counter bumped whenever a sample is recorded.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|err| err.into_inner())
    }
}

fn push_sample(data: &mut Data, speed: Speed) {
    if data.samples.len() == HISTORY {
        data.samples.pop_front();
    }
    data.samples.push_back(speed);
}

fn global() -> &'static Arc<TrafficMonitor> {
    static MONITOR: OnceLock<Arc<TrafficMonitor>> = OnceLock::new();
    MONITOR.get_or_init(|| Arc::new(TrafficMonitor::new()))
}

/// The process-wide traffic monitor.
pub fn monitor() -> Arc<TrafficMonitor> {
    Arc::clone(global())
}

/// Format a byte count with binary units (`1.5 MB`).
pub fn format_bytes(bytes: u64) -> String {
    format_size(bytes as f64)
}

/// Format a speed in bytes per second (`1.5 MB/s`).
pub fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", format_size(bytes_per_sec))
}

fn format_size(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = value.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_speeds_and_totals() {
        let monitor = TrafficMonitor::new();
        let t0 = Instant::now();

        monitor.record_at(Some((0, 0)), t0);
        monitor.record_at(Some((1000, 2048)), t0 + Duration::from_secs(1));

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.total_up, 1000);
        assert_eq!(snapshot.total_down, 2048);
        assert_eq!(snapshot.samples.len(), 1);
        assert!((snapshot.current.up - 1000.0).abs() < 1.0);
        assert!((snapshot.current.down - 2048.0).abs() < 1.0);
    }

    #[test]
    fn treats_a_counter_reset_as_a_new_baseline() {
        let monitor = TrafficMonitor::new();
        let t0 = Instant::now();
        monitor.record_at(Some((5000, 5000)), t0);
        monitor.record_at(Some((100, 100)), t0 + Duration::from_secs(1));

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.total_up, 0);
        assert_eq!(snapshot.current, Speed::default());
    }

    #[test]
    fn stopping_the_core_decays_the_speed_to_zero() {
        let monitor = TrafficMonitor::new();
        let t0 = Instant::now();
        monitor.record_at(Some((0, 0)), t0);
        monitor.record_at(Some((1024, 1024)), t0 + Duration::from_secs(1));
        assert!(monitor.snapshot().current.down > 0.0);

        monitor.record_at(None, t0 + Duration::from_secs(2));
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.current, Speed::default());
        // Totals survive.
        assert_eq!(snapshot.total_down, 1024);
    }

    #[test]
    fn history_is_bounded() {
        let monitor = TrafficMonitor::new();
        let t0 = Instant::now();
        monitor.record_at(Some((0, 0)), t0);
        let mut cumulative = 0u64;
        for step in 1..HISTORY + 10 {
            cumulative += 10;
            monitor.record_at(
                Some((cumulative, cumulative)),
                t0 + Duration::from_secs(step as u64),
            );
        }
        assert_eq!(monitor.snapshot().samples.len(), HISTORY);
    }

    #[test]
    fn reset_starts_a_fresh_baseline_without_losing_totals() {
        let monitor = TrafficMonitor::new();
        let t0 = Instant::now();
        monitor.record_at(Some((0, 0)), t0);
        monitor.record_at(Some((4096, 4096)), t0 + Duration::from_secs(1));
        let before = monitor.snapshot();
        assert!(before.total_down > 0);

        monitor.reset();
        // The new core's counters start lower; that must not be seen as a
        // decrease that swallows the following traffic.
        monitor.record_at(Some((0, 0)), t0 + Duration::from_secs(2));
        monitor.record_at(Some((512, 512)), t0 + Duration::from_secs(3));
        let after = monitor.snapshot();
        assert!(after.current.down > 0.0);
        assert!(after.total_down > before.total_down);
    }

    #[test]
    fn formats_sizes_and_speeds() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_speed(2048.0), "2.0 KB/s");
    }
}
