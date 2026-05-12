//! Bridge `tracing::warn!` / `error!` events emitted from `forge-session`
//! and its in-process dependencies into the session's event channel as
//! `forge_core::Event::LogLine`. The shell forwards those events to the
//! webview unchanged; the TS adapter (`web/packages/app/src/ipc/events.ts`)
//! routes them to `console.warn` / `console.error` so daemon errors are
//! visible in the browser devtools console.
//!
//! Scope: `forge-session` hosts exactly one [`crate::session::Session`] per
//! process, so we can install the subscriber once at startup and emit every
//! record through that single session. If the daemon ever grows to host
//! multiple sessions in one process, this module will need a span-keyed
//! router.
//!
//! Level filter: warn and above only. Info/debug/trace are intentionally
//! dropped — they belong in `events.jsonl` and the developer's stderr
//! (which `forge-session` already prints operator banners to), not in the
//! webview console.

use chrono::{DateTime, Utc};
use forge_core::LogLineLevel;
use std::fmt::Write;
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tracing::{
    field::{Field, Visit},
    Event, Metadata, Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

/// One captured tracing record, ready to be wrapped as `Event::LogLine`.
#[derive(Debug, Clone)]
pub struct LogLineRecord {
    pub at: DateTime<Utc>,
    pub level: LogLineLevel,
    pub target: String,
    pub message: String,
}

static SENDER: OnceLock<mpsc::UnboundedSender<LogLineRecord>> = OnceLock::new();

/// Install the bridge subscriber and return the receiver. Returns `None`
/// if the bridge was already installed (e.g. during a test that
/// double-bootstraps the daemon). On success, every warn-or-error tracing
/// event in this process is forwarded onto the returned channel; the caller
/// spawns a task that drains the receiver into `Session::emit`.
pub fn install() -> Option<mpsc::UnboundedReceiver<LogLineRecord>> {
    let (tx, rx) = mpsc::unbounded_channel();
    SENDER.set(tx).ok()?;
    let _ = tracing_subscriber::registry().with(BridgeLayer).try_init();
    Some(rx)
}

struct BridgeLayer;

impl<S: Subscriber> Layer<S> for BridgeLayer {
    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        // `<=` because tracing::Level orders Error < Warn < Info < ... ;
        // warn-or-error means `*level <= Level::WARN`.
        *metadata.level() <= tracing::Level::WARN
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Some(tx) = SENDER.get() else { return };
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::ERROR => LogLineLevel::Error,
            tracing::Level::WARN => LogLineLevel::Warn,
            // `enabled` filtered Info+/Debug+/Trace+ out; never reached.
            _ => return,
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let _ = tx.send(LogLineRecord {
            at: Utc::now(),
            level,
            target: metadata.target().to_string(),
            message: visitor.rendered(),
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn rendered(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields.trim_start().to_string()
        } else {
            format!("{}{}", self.message, self.fields)
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}
