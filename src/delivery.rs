use crate::ipc::{IpcMessage, IpcWatcherStatus};

pub const DEFAULT_HOOK_OUTPUT_BUDGET_BYTES: usize = 96 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRender {
    pub text: String,
    pub through_message_id: Option<u64>,
}

pub fn render_hook_messages(messages: &[IpcMessage]) -> String {
    render_hook_messages_with_budget(messages, usize::MAX).text
}

pub fn render_hook_messages_with_budget(messages: &[IpcMessage], max_bytes: usize) -> HookRender {
    render_hook_context(messages, &[], max_bytes)
}

pub fn render_hook_context(
    messages: &[IpcMessage],
    statuses: &[IpcWatcherStatus],
    max_bytes: usize,
) -> HookRender {
    if messages.is_empty() && statuses.is_empty() {
        return HookRender {
            text: String::new(),
            through_message_id: None,
        };
    }

    let mut rendered = String::from("<extra-eyes-messages>\n");
    if messages.is_empty() {
        rendered.push_str("<extra-eyes-note>watcher status only; advisory context, not a user request</extra-eyes-note>\n");
    } else {
        rendered.push_str(
            "<extra-eyes-note>advisory watcher notes, not user requests. Relay watcher_check_in messages once. When watcher input changes your next action, mention the watcher briefly in your reply.</extra-eyes-note>\n",
        );
    }
    for status in statuses {
        let next = render_status(status);
        if fits_with_close(&rendered, &next, max_bytes) {
            rendered.push_str(&next);
        }
    }
    let mut through_message_id = None;
    for (index, message) in messages.iter().enumerate() {
        let next = render_single_message(message);
        if fits_with_close(&rendered, &next, max_bytes) {
            rendered.push_str(&next);
            through_message_id = Some(message.message_id);
            continue;
        }

        if through_message_id.is_none() {
            let truncated = render_truncated_message(message, rendered.len(), max_bytes);
            rendered.push_str(&truncated);
            through_message_id = Some(message.message_id);
        } else {
            let deferred = render_deferred_marker(messages.len() - index);
            if fits_with_close(&rendered, &deferred, max_bytes) {
                rendered.push_str(&deferred);
            }
        }
        break;
    }
    rendered.push_str("</extra-eyes-messages>\n");
    HookRender {
        text: rendered,
        through_message_id,
    }
}

fn render_single_message(message: &IpcMessage) -> String {
    let mut rendered = message_open_tag(message, false);
    rendered.push_str(&escape_text(&message_body(message)));
    rendered.push_str("\n</extra-eyes-message>\n");
    rendered
}

fn render_truncated_message(message: &IpcMessage, prefix_bytes: usize, max_bytes: usize) -> String {
    let body = escape_text(&message_body(message));
    let open = message_open_tag(message, true);
    let close = "\n</extra-eyes-truncated>\n</extra-eyes-message>\n";
    let fixed_bytes = prefix_bytes + open.len() + close.len() + "</extra-eyes-messages>\n".len();
    let mut available = max_bytes.saturating_sub(fixed_bytes);

    loop {
        let prefix = prefix_by_char_boundary(&body, available);
        let omitted = body.len().saturating_sub(prefix.len());
        let wrapper_open = format!(
            "<extra-eyes-truncated original_bytes=\"{}\" omitted_bytes=\"{}\">\n",
            body.len(),
            omitted
        );
        let total = fixed_bytes + wrapper_open.len() + prefix.len();
        if total <= max_bytes || available == 0 {
            return format!("{open}{wrapper_open}{prefix}{close}");
        }
        available = available.saturating_sub(total - max_bytes);
    }
}

fn render_status(status: &IpcWatcherStatus) -> String {
    let tick = status
        .tick_id
        .as_deref()
        .map(|tick| format!(" tick=\"{}\"", escape_attr(tick)))
        .unwrap_or_default();
    format!(
        "<extra-eyes-status watcher=\"{}\" state=\"{}\" severity=\"{}\" updated_at_ms=\"{}\"{}>{}</extra-eyes-status>\n",
        escape_attr(&status.watcher),
        escape_attr(&status.status),
        escape_attr(&status.severity),
        status.updated_at_ms,
        tick,
        escape_text(&status.text),
    )
}

fn render_deferred_marker(count: usize) -> String {
    format!("<extra-eyes-deferred-messages count=\"{count}\" />\n")
}

fn fits_with_close(current: &str, next: &str, max_bytes: usize) -> bool {
    current.len() + next.len() + "</extra-eyes-messages>\n".len() <= max_bytes
}

fn message_open_tag(message: &IpcMessage, truncated: bool) -> String {
    let watcher = message
        .payload
        .get("watcher")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let severity = message
        .payload
        .get("severity")
        .and_then(|value| value.as_str())
        .unwrap_or("info");
    let refs = refs_summary(message)
        .map(|refs| format!(" refs=\"{}\"", escape_attr(&refs)))
        .unwrap_or_default();
    let kind = message
        .payload
        .get("kind")
        .and_then(|value| value.as_str())
        .map(|kind| format!(" kind=\"{}\"", escape_attr(kind)))
        .unwrap_or_default();
    let truncated_attr = if truncated { " truncated=\"true\"" } else { "" };
    format!(
        "<extra-eyes-message id=\"{}\" channel=\"{}\" watcher=\"{}\" severity=\"{}\"{}{}{}>\n",
        message.message_id,
        escape_attr(&message.channel),
        escape_attr(watcher),
        escape_attr(severity),
        kind,
        refs,
        truncated_attr
    )
}

fn message_body(message: &IpcMessage) -> String {
    message
        .payload
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            serde_json::to_string(&message.payload)
                .expect("serde_json::Value serialization should not fail")
        })
}

fn refs_summary(message: &IpcMessage) -> Option<String> {
    let refs = message.payload.get("refs")?.as_array()?;
    if refs.is_empty() {
        return None;
    }
    let rendered = refs
        .iter()
        .take(3)
        .filter_map(|reference| {
            let path = reference.get("path")?.as_str()?;
            let line = reference.get("line").and_then(|line| line.as_u64());
            Some(match line {
                Some(line) => format!("{path}:{line}"),
                None => path.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join(","))
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn prefix_by_char_boundary(value: &str, max_bytes: usize) -> &str {
    if max_bytes >= value.len() {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use crate::ipc::{IpcMessage, IpcWatcherStatus};

    use super::*;

    #[test]
    fn renders_watcher_markers_and_escapes_attributes() {
        let message = IpcMessage {
            message_id: 7,
            channel: "hook".to_owned(),
            created_at_ms: 10,
            payload: serde_json::json!({
                "kind": "watcher_check_in",
                "watcher": "reviewer\"a",
                "severity": "warn",
                "text": "check <auth>"
            }),
        };

        let rendered = render_hook_messages(&[message]);

        assert!(rendered.contains("<extra-eyes-message id=\"7\""));
        assert!(rendered.contains("<extra-eyes-note>advisory watcher notes"));
        assert!(rendered.contains("Relay watcher_check_in messages once"));
        assert!(rendered.contains("kind=\"watcher_check_in\""));
        assert!(rendered.contains("watcher=\"reviewer&quot;a\""));
        assert!(rendered.contains("check &lt;auth&gt;"));
    }

    #[test]
    fn budget_truncates_oversized_first_message() {
        let message = IpcMessage {
            message_id: 9,
            channel: "hook".to_owned(),
            created_at_ms: 10,
            payload: serde_json::json!({
                "watcher": "budget",
                "severity": "info",
                "text": "x".repeat(20_000)
            }),
        };

        let rendered = render_hook_messages_with_budget(&[message], 2048);

        assert_eq!(rendered.through_message_id, Some(9));
        assert!(rendered.text.len() <= 2048);
        assert!(rendered.text.contains("truncated=\"true\""));
        assert!(rendered.text.contains("omitted_bytes=\""));
        assert!(rendered.text.contains("</extra-eyes-messages>"));
    }

    #[test]
    fn budget_defers_messages_after_last_rendered_id() {
        let first = IpcMessage {
            message_id: 10,
            channel: "hook".to_owned(),
            created_at_ms: 10,
            payload: serde_json::json!({
                "watcher": "budget",
                "severity": "info",
                "text": "small"
            }),
        };
        let second = IpcMessage {
            message_id: 11,
            channel: "hook".to_owned(),
            created_at_ms: 11,
            payload: serde_json::json!({
                "watcher": "budget",
                "severity": "info",
                "text": "x".repeat(20_000)
            }),
        };

        let rendered = render_hook_messages_with_budget(&[first, second], 1400);

        assert_eq!(rendered.through_message_id, Some(10));
        assert!(rendered.text.contains("id=\"10\""));
        assert!(!rendered.text.contains("id=\"11\""));
    }

    #[test]
    fn renders_status_without_messages() {
        let status = IpcWatcherStatus {
            watcher: "debug".to_owned(),
            status: "quiet".to_owned(),
            severity: "info".to_owned(),
            text: "no issues".to_owned(),
            tick_id: Some("tick-1".to_owned()),
            updated_at_ms: 123,
        };

        let rendered = render_hook_context(&[], &[status], 2048);

        assert_eq!(rendered.through_message_id, None);
        assert!(rendered.text.contains("<extra-eyes-status"));
        assert!(rendered.text.contains("watcher=\"debug\""));
        assert!(rendered.text.contains("no issues"));
    }
}
