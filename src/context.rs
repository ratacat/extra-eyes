use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::paths::ProjectPaths;
use crate::state::StateStore;
use crate::watcher::WatcherContext;
use crate::Result;

pub const DEFAULT_CONTEXT_BUDGET_BYTES: usize = 512 * 1024;
const TRUNCATION_MARKER_PREFIX: &str = "\n[extra-eyes:diff-truncated omitted_bytes=";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ContextBudgetReport {
    #[serde(default)]
    pub max_bytes: Option<usize>,
    pub used_bytes: usize,
    pub truncated: bool,
    pub omitted_files: usize,
    pub omitted_conversation: usize,
    pub omitted_diff_bytes: usize,
}

pub fn build_context(paths: &ProjectPaths) -> Result<WatcherContext> {
    let root = paths.identity().root();
    let replayed = StateStore::new(paths.clone()).replay()?;
    let context = WatcherContext {
        files: git_lines(root, &["diff", "--name-only", "--relative"]),
        diff: git_text(root, &["diff", "--no-ext-diff", "--"]),
        conversation: replayed
            .conversation
            .iter()
            .map(|event| event.to_context_event())
            .collect(),
        budget: ContextBudgetReport::default(),
    };
    Ok(budget_context(context, DEFAULT_CONTEXT_BUDGET_BYTES))
}

pub fn budget_context(mut context: WatcherContext, max_bytes: usize) -> WatcherContext {
    context.budget = ContextBudgetReport {
        max_bytes: Some(max_bytes),
        used_bytes: 0,
        truncated: false,
        omitted_files: 0,
        omitted_conversation: 0,
        omitted_diff_bytes: 0,
    };
    if serialized_size(&context) <= max_bytes {
        context.budget.used_bytes = serialized_size(&context);
        return context;
    }

    let original_file_count = context.files.len();
    let original_conversation_count = context.conversation.len();
    let original_diff_len = context.diff.len();

    if serialized_size(&context) > max_bytes && !context.diff.is_empty() {
        context.diff = truncate_diff_to_fit(&context, max_bytes, original_diff_len);
    }

    while serialized_size(&context) > max_bytes && !context.conversation.is_empty() {
        context.conversation.remove(0);
    }

    while serialized_size(&context) > max_bytes && !context.files.is_empty() {
        context.files.pop();
    }

    context.budget.truncated = true;
    context.budget.omitted_files = original_file_count.saturating_sub(context.files.len());
    context.budget.omitted_conversation =
        original_conversation_count.saturating_sub(context.conversation.len());
    context.budget.omitted_diff_bytes = original_diff_len.saturating_sub(context.diff.len());
    shrink_to_fit(&mut context, max_bytes);
    context.budget.omitted_files = original_file_count.saturating_sub(context.files.len());
    context.budget.omitted_conversation =
        original_conversation_count.saturating_sub(context.conversation.len());
    context.budget.omitted_diff_bytes = original_diff_len.saturating_sub(context.diff.len());
    context.budget.used_bytes = serialized_size(&context);
    context
}

fn shrink_to_fit(context: &mut WatcherContext, max_bytes: usize) {
    while serialized_size(context) > max_bytes && !context.diff.is_empty() {
        context.diff.pop();
    }
    while serialized_size(context) > max_bytes && !context.conversation.is_empty() {
        context.conversation.remove(0);
    }
    while serialized_size(context) > max_bytes && !context.files.is_empty() {
        context.files.pop();
    }
}

fn truncate_diff_to_fit(
    context: &WatcherContext,
    max_bytes: usize,
    original_diff_len: usize,
) -> String {
    let marker = format!("{TRUNCATION_MARKER_PREFIX}{original_diff_len}]\n");
    let mut low = 0;
    let mut high = context.diff.len();
    let mut best = String::new();
    while low <= high {
        let mid = (low + high) / 2;
        let candidate = format!("{}{}", prefix_by_char_boundary(&context.diff, mid), marker);
        let mut trial = context.clone();
        trial.diff = candidate.clone();
        if serialized_size(&trial) <= max_bytes {
            best = candidate;
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    best
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

fn serialized_size(context: &WatcherContext) -> usize {
    serde_json::to_vec(context)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn git_lines(project_root: &std::path::Path, args: &[&str]) -> Vec<String> {
    git_text(project_root, args)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn git_text(project_root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).into(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::conversation::ConversationEvent;

    use super::*;

    #[test]
    fn budget_context_truncates_diff_and_preserves_recent_conversation() {
        let context = WatcherContext {
            files: vec!["src/lib.rs".to_owned()],
            diff: "x".repeat(20_000),
            conversation: vec![
                ConversationEvent {
                    text: "old".to_owned(),
                    session_id: "s".to_owned(),
                    timestamp_ms: 1,
                    ..ConversationEvent::default()
                },
                ConversationEvent {
                    text: "new".to_owned(),
                    session_id: "s".to_owned(),
                    timestamp_ms: 2,
                    ..ConversationEvent::default()
                },
            ],
            budget: ContextBudgetReport::default(),
        };

        let budgeted = budget_context(context, 1800);

        assert!(budgeted.budget.truncated);
        assert!(budgeted.budget.used_bytes <= 1800);
        assert_eq!(budgeted.files, vec!["src/lib.rs"]);
        assert_eq!(budgeted.conversation.last().unwrap().text, "new");
        assert!(budgeted.diff.contains("[extra-eyes:diff-truncated"));
    }
}
