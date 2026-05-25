use std::collections::BTreeSet;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::paths::ProjectPaths;
use crate::state::StateStore;
use crate::watcher::WatcherContext;
use crate::Result;

pub const DEFAULT_CONTEXT_BUDGET_BYTES: usize = 512 * 1024;
const TRUNCATION_MARKER_PREFIX: &str = "\n[extra-eyes:diff-truncated omitted_bytes=";
const NON_GIT_FILE_EXCERPT_BYTES: usize = 32 * 1024;

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
    let (files, diff) = if is_git_work_tree(root) {
        (git_changed_files(root), git_diff_text(root))
    } else {
        let files = crate::filewatch::FileSnapshot::scan(root)
            .map(|snapshot| snapshot.paths())
            .unwrap_or_default();
        let diff = non_git_file_excerpts(root, &files, NON_GIT_FILE_EXCERPT_BYTES);
        (files, diff)
    };
    let context = WatcherContext {
        files,
        diff,
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

fn git_changed_files(project_root: &std::path::Path) -> Vec<String> {
    let mut files = BTreeSet::new();
    for path in git_lines(project_root, &["diff", "--name-only", "--relative", "--"]) {
        insert_context_file(&mut files, path);
    }
    for path in git_lines(
        project_root,
        &["diff", "--cached", "--name-only", "--relative", "--"],
    ) {
        insert_context_file(&mut files, path);
    }
    for path in git_lines(
        project_root,
        &["ls-files", "--others", "--exclude-standard"],
    ) {
        insert_context_file(&mut files, path);
    }
    files.into_iter().collect()
}

fn insert_context_file(files: &mut BTreeSet<String>, path: String) {
    if !crate::filewatch::is_ignored(std::path::Path::new(&path)) {
        files.insert(path);
    }
}

fn git_diff_text(project_root: &std::path::Path) -> String {
    let mut sections = Vec::new();
    let unstaged = git_text(project_root, &["diff", "--no-ext-diff", "--"]);
    if !unstaged.is_empty() {
        sections.push(unstaged);
    }
    let staged = git_text(project_root, &["diff", "--cached", "--no-ext-diff", "--"]);
    if !staged.is_empty() {
        sections.push(staged);
    }
    sections.join("\n")
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

fn is_git_work_tree(project_root: &std::path::Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn non_git_file_excerpts(
    project_root: &std::path::Path,
    files: &[String],
    max_bytes: usize,
) -> String {
    let mut output = String::new();
    for file in files {
        if output.len() >= max_bytes {
            break;
        }
        let path = project_root.join(file);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let header = format!("--- {file}\n");
        if output.len() + header.len() >= max_bytes {
            break;
        }
        output.push_str(&header);
        let remaining = max_bytes.saturating_sub(output.len());
        output.push_str(prefix_by_char_boundary(&text, remaining));
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
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
