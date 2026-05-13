use forge_core::{EndReason, Event};

/// Format a single session event as a human-readable string for stdout.
/// Returns `None` for events that should be suppressed (e.g. deltas, usage ticks).
pub fn format_event(event: &Event) -> Option<String> {
    match event {
        Event::UserMessage { text, .. } => Some(format!("» {text}")),
        Event::AssistantMessage {
            text,
            model,
            stream_finalised,
            ..
        } => {
            if *stream_finalised {
                Some(format!("[{model}] {text}"))
            } else {
                None
            }
        }
        Event::AssistantDelta { delta, .. } => {
            if delta.is_empty() {
                None
            } else {
                // F-112: `delta: Arc<str>` — convert to the owned `String`
                // this function contracts to return. `to_string()` uses the
                // `Display` impl on `str`, producing one copy.
                Some(delta.to_string())
            }
        }
        Event::ToolCallStarted { tool, args, .. } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            Some(format!("  ⚙ {tool}({args_str})"))
        }
        Event::ToolCallCompleted { duration_ms, .. } => Some(format!("  ✓ done ({duration_ms}ms)")),
        Event::ToolCallApprovalRequested { preview, .. } => {
            Some(format!("  ? approval needed: {}", preview.description))
        }
        Event::SessionStarted { workspace, .. } => {
            Some(format!("session started in {}", workspace.display()))
        }
        Event::SessionEnded { reason, .. } => match reason {
            EndReason::Completed => Some("session ended: completed".into()),
            EndReason::UserExit => Some("session ended: user exit".into()),
            EndReason::Error(msg) => Some(format!("session ended: error — {msg}")),
            // F-747: window-close shutdown via the Tauri shell's
            // `session_close` IPC. The CLI never emits `Closed` itself but
            // may tail a daemon shut down by the shell.
            EndReason::Closed => Some("session ended: window closed".into()),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::types::SessionPersistence;
    use forge_core::{EndReason, Event, MessageId, ProviderId, ToolCallId};
    use std::path::PathBuf;

    fn ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    #[test]
    fn format_user_message_includes_text() {
        let e = Event::UserMessage {
            id: MessageId::new(),
            at: ts(),
            text: "hello world".into(),
            context: vec![],
            branch_parent: None,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(s.contains("hello world"), "got: {s}");
    }

    #[test]
    fn format_assistant_message_finalised_includes_text_and_model() {
        let e = Event::AssistantMessage {
            id: MessageId::new(),
            provider: ProviderId::new(),
            model: "claude-opus-4".into(),
            at: ts(),
            stream_finalised: true,
            text: "I can help with that".into(),
            branch_parent: None,
            branch_variant_index: 0,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(s.contains("I can help with that"), "got: {s}");
        assert!(s.contains("claude-opus-4"), "got: {s}");
    }

    #[test]
    fn format_assistant_message_not_finalised_is_suppressed() {
        let e = Event::AssistantMessage {
            id: MessageId::new(),
            provider: ProviderId::new(),
            model: "claude-opus-4".into(),
            at: ts(),
            stream_finalised: false,
            text: "partial...".into(),
            branch_parent: None,
            branch_variant_index: 0,
        };
        assert!(
            format_event(&e).is_none(),
            "non-finalised should be suppressed"
        );
    }

    #[test]
    fn format_tool_call_started_includes_tool_name() {
        let e = Event::ToolCallStarted {
            id: ToolCallId::new(),
            msg: MessageId::new(),
            tool: "fs.read".into(),
            args: serde_json::json!({"path": "/tmp/test.txt"}),
            at: ts(),
            parallel_group: None,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(s.contains("fs.read"), "got: {s}");
    }

    #[test]
    fn format_session_ended_completed() {
        let e = Event::SessionEnded {
            at: ts(),
            reason: EndReason::Completed,
            archived: false,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(
            s.to_lowercase().contains("completed") || s.to_lowercase().contains("ended"),
            "got: {s}"
        );
    }

    #[test]
    fn format_session_ended_error_includes_message() {
        let e = Event::SessionEnded {
            at: ts(),
            reason: EndReason::Error("provider timeout".into()),
            archived: false,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(s.contains("provider timeout"), "got: {s}");
    }

    #[test]
    fn format_session_started_includes_workspace() {
        let e = Event::SessionStarted {
            at: ts(),
            workspace: PathBuf::from("/home/user/myproject"),
            agent: None,
            persistence: SessionPersistence::Persist,
        };
        let s = format_event(&e).expect("should produce output");
        assert!(s.contains("myproject"), "got: {s}");
    }
}
