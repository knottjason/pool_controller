//! In-process ring buffer of recent tracing log lines for the HTTP dashboard.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Fixed-capacity ring of formatted log lines.
#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl LogBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            lines: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }))
    }

    /// Resize the ring; drops oldest lines if over the new capacity.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Newest-last snapshot, optionally truncated to the last `limit` lines.
    #[must_use]
    pub fn snapshot(&self, limit: Option<usize>) -> Vec<String> {
        let n = self.lines.len();
        let take = limit.map_or(n, |l| l.min(n));
        self.lines
            .iter()
            .skip(n.saturating_sub(take))
            .cloned()
            .collect()
    }
}

/// Tracing layer that appends formatted events to a [`LogBuffer`].
#[derive(Clone)]
pub struct LogBufferLayer {
    buffer: Arc<Mutex<LogBuffer>>,
}

impl LogBufferLayer {
    #[must_use]
    pub const fn new(buffer: Arc<Mutex<LogBuffer>>) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for LogBufferLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let mut line = format!("{} {} {}", chrono_like_now(), meta.level(), meta.target());
        if !visitor.message.is_empty() {
            let _ = write!(line, ": {}", visitor.message);
        }
        if !visitor.fields.is_empty() {
            let _ = write!(line, " {}", visitor.fields);
        }
        if let Ok(mut guard) = self.buffer.lock() {
            guard.push(line);
        }
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: String,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.append_field(field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            if self.message.starts_with('"')
                && self.message.ends_with('"')
                && self.message.len() >= 2
            {
                self.message = self.message[1..self.message.len() - 1]
                    .replace("\\\"", "\"")
                    .replace("\\n", "\n");
            }
        } else {
            self.append_field(field.name(), &format!("{value:?}"));
        }
    }
}

impl FieldVisitor {
    fn append_field(&mut self, name: &str, value: &str) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(self.fields, "{name}={value}");
    }
}

/// Compact local timestamp without pulling in chrono.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "unknown".into();
    };
    let secs = dur.as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let s = secs % 60;
    format!("{hours:02}:{mins:02}:{s:02}")
}
