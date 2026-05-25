use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::{EyesError, Result};

pub const INSTALLED_EVENTS: [&str; 3] = ["UserPromptSubmit", "PreToolUse", "Stop"];
const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 2;
const USER_PROMPT_HOOK_TIMEOUT_SECONDS: u64 = 20;

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeInstallResult {
    pub settings_path: PathBuf,
    pub eyes_bin: PathBuf,
    pub installed_events: Vec<String>,
}

pub fn install_claude_hooks(settings_path: &Path, eyes_bin: &Path) -> Result<ClaudeInstallResult> {
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let original = match fs::read_to_string(settings_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => return Err(error.into()),
    };
    let mut root = serde_json::from_str::<Value>(&original)?;
    install_hooks_value(&mut root, eyes_bin)?;
    atomic_write(settings_path, &serde_json::to_string_pretty(&root)?)?;

    Ok(ClaudeInstallResult {
        settings_path: settings_path.to_path_buf(),
        eyes_bin: eyes_bin.to_path_buf(),
        installed_events: INSTALLED_EVENTS
            .iter()
            .map(|event| (*event).to_owned())
            .collect(),
    })
}

fn install_hooks_value(root: &mut Value, eyes_bin: &Path) -> Result<()> {
    let root = root.as_object_mut().ok_or_else(|| {
        EyesError::Config("Claude Code settings must be a JSON object".to_owned())
    })?;
    let hooks = root
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks = hooks.as_object_mut().ok_or_else(|| {
        EyesError::Config("Claude Code settings field `hooks` must be a JSON object".to_owned())
    })?;

    for event in INSTALLED_EVENTS {
        let groups = hooks
            .entry(event.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let groups = groups.as_array_mut().ok_or_else(|| {
            EyesError::Config(format!(
                "Claude Code settings hooks.{event} must be an array"
            ))
        })?;
        remove_installed_hooks(groups);
        groups.push(installed_hook_group(eyes_bin, event));
    }

    Ok(())
}

fn remove_installed_hooks(groups: &mut Vec<Value>) {
    let mut kept = Vec::new();
    for mut group in std::mem::take(groups) {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            kept.push(group);
            continue;
        };
        hooks.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .map(is_installed_command)
                .unwrap_or(false)
        });
        if !hooks.is_empty() {
            kept.push(group);
        }
    }
    *groups = kept;
}

fn installed_hook_group(eyes_bin: &Path, event: &str) -> Value {
    serde_json::json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": installed_command(eyes_bin, event),
            "timeout": installed_timeout(event)
        }]
    })
}

fn installed_command(eyes_bin: &Path, event: &str) -> String {
    format!(
        "{} hook claude-code --integration extra-eyes --event {event} --limit 1000 --project .",
        shell_quote(&eyes_bin.display().to_string())
    )
}

fn is_installed_command(command: &str) -> bool {
    command.contains(" hook claude-code ")
        && command.contains(" --integration extra-eyes ")
        && INSTALLED_EVENTS
            .iter()
            .any(|event| command.contains(&format!(" --event {event} ")))
}

fn installed_timeout(event: &str) -> u64 {
    if event == "UserPromptSubmit" {
        USER_PROMPT_HOOK_TIMEOUT_SECONDS
    } else {
        DEFAULT_HOOK_TIMEOUT_SECONDS
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| EyesError::Config("Claude settings path must have a parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| EyesError::Config("Claude settings path must have a file name".to_owned()))?
        .to_string_lossy();
    let temp_path = parent.join(format!(".{file_name}.extra-eyes.tmp"));
    fs::write(&temp_path, contents)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp_path, metadata.permissions())?;
    }
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn installs_claude_hooks_idempotently() {
        let temp = TempDir::new().unwrap();
        let settings = temp.path().join("settings.json");
        let eyes_bin = temp.path().join("bin/eyes");
        fs::write(
            &settings,
            r#"{
  "permissions": {
    "allow": ["Bash(git status:*)"]
  },
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": "existing",
        "hooks": [
          {
            "type": "command",
            "command": "echo existing",
            "timeout": 1
          }
        ]
      }
    ]
  }
}"#,
        )
        .unwrap();

        let first = install_claude_hooks(&settings, &eyes_bin).unwrap();
        assert_eq!(
            first.installed_events,
            ["UserPromptSubmit", "PreToolUse", "Stop"]
        );
        let second = install_claude_hooks(&settings, &eyes_bin).unwrap();
        assert_eq!(
            second.installed_events,
            ["UserPromptSubmit", "PreToolUse", "Stop"]
        );

        let written = fs::read_to_string(&settings).unwrap();
        assert_eq!(
            written
                .matches("hook claude-code --integration extra-eyes --event UserPromptSubmit")
                .count(),
            1
        );
        assert_eq!(
            written
                .matches("hook claude-code --integration extra-eyes --event PreToolUse")
                .count(),
            1
        );
        assert_eq!(
            written
                .matches("hook claude-code --integration extra-eyes --event Stop")
                .count(),
            1
        );
        assert!(written.contains("\"permissions\""));
        assert!(written.contains("echo existing"));
        assert_eq!(
            written
                .lines()
                .filter(|line| line.trim().trim_end_matches(',') == "\"timeout\": 20")
                .count(),
            1
        );
        assert_eq!(
            written
                .lines()
                .filter(|line| line.trim().trim_end_matches(',') == "\"timeout\": 2")
                .count(),
            2
        );
    }
}
