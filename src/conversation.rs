use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EyesError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ConversationEvent {
    pub harness: String,
    pub event: String,
    pub role: String,
    pub text: String,
    pub source: String,
    pub session_id: String,
    pub timestamp_ms: u64,
}

pub fn normalize_hook_payload(
    harness: &str,
    event: &str,
    payload: Value,
    observed_at_ms: u64,
) -> Result<ConversationEvent> {
    validate_harness(harness)?;
    let event_name = normalize_event_name(event)?;
    let role = infer_role(&event_name);
    let text = extract_text(&payload, role)?;
    let source = extract_string(&payload, &["source", "event_source", "origin"])
        .unwrap_or_else(|| event_name.clone());
    let session_id = extract_string(&payload, &["session_id", "sessionId", "session"])
        .ok_or_else(|| EyesError::Protocol("conversation payload missing session_id".to_owned()))?;
    let timestamp_ms = extract_u64(&payload, &["timestamp_ms", "created_at_ms", "time_ms"])
        .unwrap_or(observed_at_ms);

    Ok(ConversationEvent {
        harness: harness.to_owned(),
        event: event_name,
        role: role.to_owned(),
        text,
        source,
        session_id,
        timestamp_ms,
    })
}

fn validate_harness(harness: &str) -> Result<()> {
    match harness {
        "claude-code" | "codex" | "pi" => Ok(()),
        _ => Err(EyesError::Protocol(format!(
            "unsupported conversation harness '{harness}'"
        ))),
    }
}

fn normalize_event_name(event: &str) -> Result<String> {
    let normalized = event.trim().replace('-', "_").to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(EyesError::Protocol(
            "conversation event cannot be empty".to_owned(),
        ));
    }
    Ok(normalized)
}

fn infer_role(event: &str) -> &'static str {
    if event.contains("stop")
        || event.contains("assistant")
        || event.contains("session_shutdown")
        || event.contains("output")
    {
        "assistant"
    } else {
        "user"
    }
}

fn extract_text(payload: &Value, role: &str) -> Result<String> {
    let candidates: &[&str] = if role == "assistant" {
        &[
            "last_assistant_message",
            "assistant_message",
            "assistant",
            "text",
            "message",
            "output",
        ]
    } else {
        &["prompt", "text", "message", "input"]
    };
    extract_string(payload, candidates)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| EyesError::Protocol("conversation payload missing text".to_owned()))
}

fn extract_string(payload: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = payload.get(*key).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
        if let Some(text) = payload
            .get("event")
            .and_then(|event| event.get(*key))
            .and_then(Value::as_str)
        {
            return Some(text.to_owned());
        }
    }
    None
}

fn extract_u64(payload: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_u64) {
            return Some(value);
        }
        if let Some(value) = payload
            .get("event")
            .and_then(|event| event.get(*key))
            .and_then(Value::as_u64)
        {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_claude_user_prompt_payload() {
        let event = normalize_hook_payload(
            "claude-code",
            "UserPromptSubmit",
            serde_json::json!({
                "session_id": "cc-session",
                "transcript_path": "/tmp/cc-session.jsonl",
                "cwd": "/tmp/project",
                "permission_mode": "dontAsk",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "fix the tests",
            }),
            999,
        )
        .unwrap();

        assert_eq!(event.role, "user");
        assert_eq!(event.text, "fix the tests");
        assert_eq!(event.source, "userpromptsubmit");
        assert_eq!(event.session_id, "cc-session");
        assert_eq!(event.timestamp_ms, 999);
    }

    #[test]
    fn normalizes_codex_stop_payload() {
        let event = normalize_hook_payload(
            "codex",
            "Stop",
            serde_json::json!({
                "session_id": "codex-session",
                "last_assistant_message": "done",
            }),
            456,
        )
        .unwrap();

        assert_eq!(event.role, "assistant");
        assert_eq!(event.text, "done");
        assert_eq!(event.source, "stop");
        assert_eq!(event.timestamp_ms, 456);
    }

    #[test]
    fn normalizes_pi_input_payload() {
        let event = normalize_hook_payload(
            "pi",
            "input",
            serde_json::json!({
                "session_id": "pi-session",
                "event": {"text": "hello", "source": "interactive"},
            }),
            789,
        )
        .unwrap();

        assert_eq!(event.role, "user");
        assert_eq!(event.text, "hello");
        assert_eq!(event.source, "interactive");
        assert_eq!(event.session_id, "pi-session");
    }
}
