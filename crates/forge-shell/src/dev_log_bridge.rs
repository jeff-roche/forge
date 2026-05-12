//! Bridge `tracing::*` events into `tauri-plugin-log` (and from there into
//! the webview console) for debug builds of `forge-shell`.
//!
//! ## Why a custom layer?
//!
//! `tauri-plugin-log` captures records emitted through the `log` crate.
//! `forge-shell` emits through `tracing`. The two ecosystems don't share a
//! global emitter, so without a bridge every `tracing::warn!` / `error!`
//! call in `forge-shell` is dropped on the floor (no subscriber → silent).
//! This module installs a `tracing-subscriber::Layer` that, on every
//! tracing event, forwards the rendered message to `log::log!` at the
//! matching level. The plugin's registered global logger receives that
//! record and pipes it to whichever targets are configured — `Stdout`
//! always, and `Webview` in debug builds — so the user sees backend warns
//! and errors in the browser devtools console alongside frontend ones.

use std::fmt::Write;

use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, Layer};

/// Forwards each `tracing::Event` to `log::log!` at the matching level.
/// Drops the daemon's structured fields into the rendered message as
/// `key=value` pairs after the human-readable message, mirroring the
/// default `tracing-subscriber::fmt` output style. Span context is
/// intentionally not threaded — `tauri-plugin-log` flattens to single
/// records and the webview console renders a flat stream, so the cost
/// of stitching spans back together isn't worth the noise.
pub struct LogBridge;

impl<S: Subscriber> Layer<S> for LogBridge {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::ERROR => log::Level::Error,
            tracing::Level::WARN => log::Level::Warn,
            tracing::Level::INFO => log::Level::Info,
            tracing::Level::DEBUG => log::Level::Debug,
            tracing::Level::TRACE => log::Level::Trace,
        };
        // Filter check up-front so we don't render the message body for
        // levels the global logger will throw away.
        if !log::log_enabled!(target: metadata.target(), level) {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        log::log!(target: metadata.target(), level, "{}", visitor.rendered());
    }
}

#[derive(Default)]
pub(crate) struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    pub(crate) fn rendered(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            // No human-readable `%message`; emit fields alone with the
            // leading space stripped so the line doesn't start with " ".
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
