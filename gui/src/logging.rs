//! In-memory log capture for the log window.
//!
//! The application already prints `tracing` output to the console; this module
//! adds a second `tracing` layer that keeps a bounded ring buffer of rendered
//! lines for the GUI to display.
//!
//! Timestamps are relative to process start (`HH:MM:SS`) rather than wall-clock
//! time: the standard library exposes no local-time API, and pulling in a
//! timezone crate for a log prefix is not worth it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Maximum number of retained lines.
const MAX_LINES: usize = 2000;
/// Maximum length of a single rendered line, in bytes.
const MAX_LINE_LEN: usize = 2048;

/// One captured log line.
#[derive(Clone, Debug)]
pub struct LogLine {
    /// Time since startup, formatted `HH:MM:SS`.
    pub time: String,
    /// Severity of the event.
    pub level: Level,
    /// Rendered message, including any structured fields.
    /// Module path of the event, e.g. `juicity_gui::core`.
    pub target: String,
    pub message: String,
}

struct Buffer {
    lines: VecDeque<LogLine>,
    dropped: u64,
}

/// Process-wide ring buffer of recent log lines.
pub struct LogBuffer {
    buffer: Mutex<Buffer>,
    /// Bumped whenever the buffer changes so the UI can detect updates cheaply.
    version: AtomicU64,
    start: Instant,
}

impl LogBuffer {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Mutex::new(Buffer {
                lines: VecDeque::new(),
                dropped: 0,
            }),
            version: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    pub(crate) fn push(&self, level: Level, target: &str, message: String) {
        let line = LogLine {
            time: elapsed(self.start),
            level,
            target: target.to_string(),
            message,
        };
        {
            let mut buffer = self.lock();
            if buffer.lines.len() == MAX_LINES {
                buffer.lines.pop_front();
                buffer.dropped += 1;
            }
            buffer.lines.push_back(line);
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Copy of the retained lines, oldest first.
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lock().lines.iter().cloned().collect()
    }

    /// How many lines were discarded because the buffer was full.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Monotonic counter bumped whenever a line is added or the buffer is cleared.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Drop every retained line.
    pub fn clear(&self) {
        {
            let mut buffer = self.lock();
            buffer.lines.clear();
            buffer.dropped = 0;
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Buffer> {
        self.buffer.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Format an elapsed duration as `HH:MM:SS`.
fn elapsed(start: Instant) -> String {
    let secs = start.elapsed().as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

fn global() -> &'static Arc<LogBuffer> {
    static BUFFER: OnceLock<Arc<LogBuffer>> = OnceLock::new();
    BUFFER.get_or_init(|| Arc::new(LogBuffer::new()))
}

/// The process-wide log buffer used by the log window.
pub fn buffer() -> Arc<LogBuffer> {
    Arc::clone(global())
}

/// Install the tracing subscriber: the usual console output plus the capture
/// layer that feeds the log window.
pub fn init(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(CaptureLayer {
            buffer: Arc::clone(global()),
        })
        .init();
}

/// `tracing` layer that renders every event into the [`LogBuffer`].
struct CaptureLayer {
    buffer: Arc<LogBuffer>,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let message = if visitor.message.is_empty() {
            event.metadata().name().to_string()
        } else {
            visitor.message
        };
        let metadata = event.metadata();
        self.buffer
            .push(*metadata.level(), metadata.target(), message);
    }
}

/// Renders the `message` field plus any structured fields that follow it.
#[derive(Default)]
struct EventVisitor {
    message: String,
}

impl EventVisitor {
    fn append(&mut self, text: &str) {
        if self.message.len() >= MAX_LINE_LEN {
            return;
        }
        if !self.message.is_empty() {
            self.message.push(' ');
        }
        let room = MAX_LINE_LEN - self.message.len();
        if text.len() <= room {
            self.message.push_str(text);
        } else {
            // Truncate on a char boundary.
            let mut end = room;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            self.message.push_str(&text[..end]);
            self.message.push('…');
        }
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.append(value);
        } else {
            self.append(&format!("{}={value}", field.name()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `message` is recorded as `format_args!`, whose `Debug` prints the
            // formatted text without surrounding quotes.
            self.append(&format!("{value:?}"));
        } else {
            self.append(&format!("{}={value:?}", field.name()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_formats_hours_minutes_seconds() {
        let start = Instant::now();
        assert_eq!(elapsed(start), "00:00:00");
    }

    #[test]
    fn buffer_retains_newest_lines_and_reports_drops() {
        let buffer = LogBuffer::new();
        for i in 0..MAX_LINES + 5 {
            buffer.push(Level::INFO, "test", format!("line {i}"));
        }
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(buffer.dropped(), 5);
        assert_eq!(
            lines.last().unwrap().message,
            format!("line {}", MAX_LINES + 4)
        );
        assert_eq!(lines.first().unwrap().message, "line 5");
    }

    #[test]
    fn clear_empties_the_buffer_and_bumps_the_version() {
        let buffer = LogBuffer::new();
        buffer.push(Level::WARN, "test", "boom".to_string());
        let before = buffer.version();
        buffer.clear();
        assert!(buffer.snapshot().is_empty());
        assert!(buffer.version() > before);
    }

    #[test]
    fn visitor_appends_message_then_fields_and_truncates() {
        let mut visitor = EventVisitor::default();
        visitor.append("hello");
        visitor.append("world");
        assert_eq!(visitor.message, "hello world");

        let mut visitor = EventVisitor::default();
        visitor.append(&"x".repeat(MAX_LINE_LEN + 100));
        assert!(visitor.message.len() <= MAX_LINE_LEN + '…'.len_utf8());
    }
}
