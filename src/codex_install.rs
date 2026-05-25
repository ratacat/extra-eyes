use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml::Value as TomlValue;

use crate::codex_trust::{self, CodexHookTrustEntry};
use crate::{EyesError, Result};

pub const INSTALLED_EVENTS: [&str; 4] = ["SessionStart", "UserPromptSubmit", "PreToolUse", "Stop"];
const BEGIN_MARKER: &str = "# BEGIN EXTRA EYES CODEX HOOKS";
const END_MARKER: &str = "# END EXTRA EYES CODEX HOOKS";
const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 2;
const USER_PROMPT_HOOK_TIMEOUT_SECONDS: u64 = 20;

#[derive(Debug, Clone, Serialize)]
pub struct CodexInstallResult {
    pub config_path: PathBuf,
    pub eyes_bin: PathBuf,
    pub installed_events: Vec<String>,
    pub trust_entries: Vec<CodexHookTrustEntry>,
    pub warnings: Vec<String>,
}

pub fn install_codex_hooks(config_path: &Path, eyes_bin: &Path) -> Result<CodexInstallResult> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let original = match fs::read_to_string(config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let base = strip_managed_block(&original)?;
    let warnings = install_warnings(config_path, &base);
    let hooks_only = with_managed_block(&base, &managed_block(eyes_bin, &[]));
    let trust_entries = codex_trust::trust_entries_for_config_contents(config_path, &hooks_only)?
        .into_iter()
        .filter(|entry| is_installed_command(&entry.command))
        .collect::<Vec<_>>();
    let installed = with_managed_block(&base, &managed_block(eyes_bin, &trust_entries));
    codex_trust::trust_entries_for_config_contents(config_path, &installed)?;
    atomic_write(config_path, &installed)?;
    Ok(CodexInstallResult {
        config_path: config_path.to_path_buf(),
        eyes_bin: eyes_bin.to_path_buf(),
        installed_events: INSTALLED_EVENTS
            .iter()
            .map(|event| (*event).to_owned())
            .collect(),
        trust_entries,
        warnings,
    })
}

fn is_installed_command(command: &str) -> bool {
    command.contains(" hook codex ")
        && command.contains(" --integration extra-eyes ")
        && INSTALLED_EVENTS
            .iter()
            .any(|event| command.contains(&format!(" --event {event} ")))
}

fn managed_block(eyes_bin: &Path, trust_entries: &[CodexHookTrustEntry]) -> String {
    let mut block = String::new();
    block.push_str(BEGIN_MARKER);
    block.push('\n');

    for event in INSTALLED_EVENTS {
        let command = installed_command(eyes_bin, event);
        block.push('\n');
        block.push_str(&format!("[[hooks.{event}]]\n"));
        block.push_str("matcher = \"\"\n\n");
        block.push_str(&format!("[[hooks.{event}.hooks]]\n"));
        block.push_str("type = \"command\"\n");
        block.push_str(&format!("command = {}\n", toml_string(&command)));
        block.push_str("async = false\n");
        block.push_str(&format!("timeout = {}\n", installed_timeout(event)));
    }

    for entry in trust_entries {
        block.push('\n');
        block.push_str(&format!("[hooks.state.{}]\n", toml_string(&entry.key)));
        block.push_str(&format!(
            "trusted_hash = {}\n",
            toml_string(&entry.trusted_hash)
        ));
    }

    block.push('\n');
    block.push_str(END_MARKER);
    block.push('\n');
    block
}

fn installed_command(eyes_bin: &Path, event: &str) -> String {
    format!(
        "{} hook codex --integration extra-eyes --event {event} --limit 1000 --project .",
        shell_quote(&eyes_bin.display().to_string())
    )
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

fn toml_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{08}' => quoted.push_str("\\b"),
            '\u{0c}' => quoted.push_str("\\f"),
            ch if ch < '\u{20}' => quoted.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn strip_managed_block(contents: &str) -> Result<String> {
    if !contents.contains(BEGIN_MARKER) && !contents.contains(END_MARKER) {
        return Ok(contents.to_owned());
    }

    let mut output = String::new();
    let mut in_managed_block = false;
    for line in contents.lines() {
        match line.trim() {
            BEGIN_MARKER if !in_managed_block => {
                in_managed_block = true;
            }
            BEGIN_MARKER => {
                return Err(EyesError::Config(
                    "Codex config contains nested Extra Eyes managed blocks".to_owned(),
                ));
            }
            END_MARKER if in_managed_block => {
                in_managed_block = false;
            }
            END_MARKER => {
                return Err(EyesError::Config(
                    "Codex config contains an Extra Eyes end marker without a begin marker"
                        .to_owned(),
                ));
            }
            _ if !in_managed_block => {
                output.push_str(line);
                output.push('\n');
            }
            _ => {}
        }
    }

    if in_managed_block {
        return Err(EyesError::Config(
            "Codex config contains an unterminated Extra Eyes managed block".to_owned(),
        ));
    }

    Ok(output.trim_end().to_owned())
}

fn with_managed_block(base: &str, block: &str) -> String {
    let mut installed = base.trim_end().to_owned();
    if !installed.is_empty() {
        installed.push_str("\n\n");
    }
    installed.push_str(block.trim_end());
    installed.push('\n');
    installed
}

fn install_warnings(config_path: &Path, base_config: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if config_path
        .parent()
        .map(|parent| parent.join("hooks.json").exists())
        .unwrap_or(false)
    {
        warnings.push(
            "Codex hooks.json also exists next to config.toml; Codex may merge both hook sources"
                .to_owned(),
        );
    }

    if let Ok(root) = base_config.parse::<TomlValue>() {
        let features = root.get("features");
        let hooks_disabled = features
            .and_then(|features| features.get("hooks"))
            .and_then(TomlValue::as_bool)
            == Some(false);
        let codex_hooks_disabled = features
            .and_then(|features| features.get("codex_hooks"))
            .and_then(TomlValue::as_bool)
            == Some(false);
        if hooks_disabled || codex_hooks_disabled {
            warnings.push(
                "Codex hook feature flag appears disabled; installed hooks may not run".to_owned(),
            );
        }
    }

    warnings
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| EyesError::Config("Codex config path must have a parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| EyesError::Config("Codex config path must have a file name".to_owned()))?
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
    fn installs_hooks_and_trust_state_idempotently() {
        let temp = TempDir::new().unwrap();
        let config = temp.path().join("config.toml");
        let eyes_bin = temp.path().join("eyes bin");
        fs::write(
            &config,
            r#"# user comment must survive
[features]
hooks = true

[hooks]

[[hooks.UserPromptSubmit]]
matcher = "existing"

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "echo existing"
"#,
        )
        .unwrap();

        let first = install_codex_hooks(&config, &eyes_bin).unwrap();
        assert_eq!(first.trust_entries.len(), 4);
        assert!(first.warnings.is_empty());
        let second = install_codex_hooks(&config, &eyes_bin).unwrap();
        assert_eq!(second.trust_entries.len(), 4);

        let written = fs::read_to_string(&config).unwrap();
        assert!(written.contains("# user comment must survive"));
        assert_eq!(
            written
                .matches("hook codex --integration extra-eyes --event SessionStart")
                .count(),
            1
        );
        assert_eq!(
            written
                .matches("hook codex --integration extra-eyes --event UserPromptSubmit")
                .count(),
            1
        );
        assert_eq!(
            written
                .matches("hook codex --integration extra-eyes --event PreToolUse")
                .count(),
            1
        );
        assert_eq!(
            written
                .matches("hook codex --integration extra-eyes --event Stop")
                .count(),
            1
        );
        assert!(written.contains("command = \"echo existing\""));
        assert_eq!(written.matches("async = false").count(), 4);
        assert_eq!(
            written
                .lines()
                .filter(|line| line.trim() == "timeout = 20")
                .count(),
            1
        );
        assert_eq!(
            written
                .lines()
                .filter(|line| line.trim() == "timeout = 2")
                .count(),
            3
        );
        assert!(written.contains("trusted_hash = \"sha256:"));
    }
}
