use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use toml::Value as TomlValue;

use crate::{EyesError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexHookTrustEntry {
    pub key: String,
    pub trusted_hash: String,
    pub event_name: &'static str,
    pub group_index: usize,
    pub handler_index: usize,
    pub command: String,
}

#[derive(Debug, Default, Deserialize)]
struct CodexConfigToml {
    #[serde(default)]
    hooks: CodexHooksToml,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct CodexHooksToml {
    #[serde(rename = "PreToolUse", default)]
    pre_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PermissionRequest", default)]
    permission_request: Vec<MatcherGroup>,
    #[serde(rename = "PostToolUse", default)]
    post_tool_use: Vec<MatcherGroup>,
    #[serde(rename = "PreCompact", default)]
    pre_compact: Vec<MatcherGroup>,
    #[serde(rename = "PostCompact", default)]
    post_compact: Vec<MatcherGroup>,
    #[serde(rename = "SessionStart", default)]
    session_start: Vec<MatcherGroup>,
    #[serde(rename = "UserPromptSubmit", default)]
    user_prompt_submit: Vec<MatcherGroup>,
    #[serde(rename = "Stop", default)]
    stop: Vec<MatcherGroup>,
}

impl CodexHooksToml {
    fn event_groups(&self) -> [(&'static str, &'static str, bool, &[MatcherGroup]); 8] {
        [
            ("PreToolUse", "pre_tool_use", true, &self.pre_tool_use),
            (
                "PermissionRequest",
                "permission_request",
                true,
                &self.permission_request,
            ),
            ("PostToolUse", "post_tool_use", true, &self.post_tool_use),
            ("PreCompact", "pre_compact", true, &self.pre_compact),
            ("PostCompact", "post_compact", true, &self.post_compact),
            ("SessionStart", "session_start", true, &self.session_start),
            (
                "UserPromptSubmit",
                "user_prompt_submit",
                false,
                &self.user_prompt_submit,
            ),
            ("Stop", "stop", false, &self.stop),
        ]
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookHandlerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
enum HookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default, rename = "timeout")]
        timeout_sec: Option<u64>,
        #[serde(default)]
        r#async: bool,
        #[serde(default, rename = "statusMessage")]
        status_message: Option<String>,
    },
    #[serde(rename = "prompt")]
    Prompt {},
    #[serde(rename = "agent")]
    Agent {},
}

#[derive(Serialize)]
struct NormalizedHookIdentity {
    event_name: &'static str,
    #[serde(flatten)]
    group: MatcherGroup,
}

pub fn trust_entries_for_config(config_path: &Path) -> Result<Vec<CodexHookTrustEntry>> {
    let contents = fs::read_to_string(config_path)?;
    trust_entries_for_config_contents(config_path, &contents)
}

pub fn trust_entries_for_config_contents(
    config_path: &Path,
    contents: &str,
) -> Result<Vec<CodexHookTrustEntry>> {
    let canonical_config_path = canonicalize_config_path(config_path)?;
    let parsed: CodexConfigToml = toml::from_str(&contents)?;
    let key_source = canonical_config_path.display().to_string();
    let mut entries = Vec::new();

    for (event_name, event_key, uses_matcher, groups) in parsed.hooks.event_groups() {
        for (group_index, group) in groups.iter().enumerate() {
            let matcher = uses_matcher.then(|| group.matcher.clone()).flatten();
            for (handler_index, handler) in group.hooks.iter().enumerate() {
                let HookHandlerConfig::Command {
                    command,
                    timeout_sec,
                    r#async,
                    status_message,
                } = handler
                else {
                    continue;
                };
                if *r#async || command.trim().is_empty() {
                    continue;
                }

                let normalized_handler = HookHandlerConfig::Command {
                    command: command.clone(),
                    timeout_sec: Some(timeout_sec.unwrap_or(600).max(1)),
                    r#async: false,
                    status_message: status_message.clone(),
                };
                let trusted_hash =
                    command_hook_hash(event_key, matcher.clone(), group, normalized_handler)?;
                entries.push(CodexHookTrustEntry {
                    key: format!("{key_source}:{event_key}:{group_index}:{handler_index}"),
                    trusted_hash,
                    event_name,
                    group_index,
                    handler_index,
                    command: command.clone(),
                });
            }
        }
    }

    Ok(entries)
}

fn canonicalize_config_path(config_path: &Path) -> Result<PathBuf> {
    if config_path.exists() {
        return Ok(fs::canonicalize(config_path)?);
    }
    let parent = config_path
        .parent()
        .ok_or_else(|| EyesError::Config("Codex config path must have a parent".to_owned()))?;
    let file_name = config_path
        .file_name()
        .ok_or_else(|| EyesError::Config("Codex config path must have a file name".to_owned()))?;
    Ok(fs::canonicalize(parent)?.join(file_name))
}

pub fn write_trust_state(
    state_config_path: &Path,
    entries: &[CodexHookTrustEntry],
) -> Result<usize> {
    let mut root = if state_config_path.exists() {
        fs::read_to_string(state_config_path)?.parse::<TomlValue>()?
    } else {
        TomlValue::Table(toml::map::Map::new())
    };
    let root_table = root
        .as_table_mut()
        .ok_or_else(|| EyesError::Config("Codex config root must be a TOML table".to_owned()))?;
    let hooks_table = ensure_table(root_table, "hooks")?;
    let state_table = ensure_table(hooks_table, "state")?;

    for entry in entries {
        let entry_table = ensure_table(state_table, &entry.key)?;
        entry_table.insert(
            "trusted_hash".to_owned(),
            TomlValue::String(entry.trusted_hash.clone()),
        );
    }

    if let Some(parent) = state_config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(state_config_path, toml::to_string_pretty(&root)?)?;
    Ok(entries.len())
}

fn ensure_table<'a>(
    table: &'a mut toml::map::Map<String, TomlValue>,
    key: &str,
) -> Result<&'a mut toml::map::Map<String, TomlValue>> {
    if !table.contains_key(key) {
        table.insert(key.to_owned(), TomlValue::Table(toml::map::Map::new()));
    }
    table
        .get_mut(key)
        .and_then(TomlValue::as_table_mut)
        .ok_or_else(|| EyesError::Config(format!("TOML key '{key}' must be a table")))
}

fn command_hook_hash(
    event_key: &'static str,
    matcher: Option<String>,
    group: &MatcherGroup,
    normalized_handler: HookHandlerConfig,
) -> Result<String> {
    let mut normalized_group = group.clone();
    normalized_group.matcher = matcher;
    normalized_group.hooks = vec![normalized_handler];
    let identity = NormalizedHookIdentity {
        event_name: event_key,
        group: normalized_group,
    };
    let value = TomlValue::try_from(identity)
        .map_err(|error| EyesError::Config(format!("failed to normalize Codex hook: {error}")))?;
    Ok(version_for_toml(&value))
}

fn version_for_toml(value: &TomlValue) -> String {
    let json = serde_json::to_value(value).unwrap_or(JsonValue::Null);
    let canonical = canonical_json(&json);
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    format!("sha256:{}", hex::encode(hash))
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, canonical_json(value));
                }
            }
            JsonValue::Object(sorted)
        }
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn computes_codex_hook_keys_and_hashes() {
        let temp = TempDir::new().unwrap();
        let config = temp.path().join("config.toml");
        fs::write(
            &config,
            r#"[hooks]

[[hooks.UserPromptSubmit]]
matcher = "ignored"

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "tee -a /tmp/upshook.jsonl > /dev/null"

[[hooks.SessionStart]]
matcher = "startup|resume"

[[hooks.SessionStart.hooks]]
type = "command"
command = "echo session"
timeout = 0
statusMessage = "starting"
"#,
        )
        .unwrap();

        let canonical = fs::canonicalize(&config).unwrap();
        let entries = trust_entries_for_config(&config).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].key,
            format!("{}:session_start:0:0", canonical.display())
        );
        assert_eq!(
            entries[1].key,
            format!("{}:user_prompt_submit:0:0", canonical.display())
        );
        assert_eq!(
            entries[0].trusted_hash,
            "sha256:9f098ea01cc5b8fbd65665b454372beb824596b9479506bc8eef27eef91ebbba"
        );
        assert_eq!(
            entries[1].trusted_hash,
            "sha256:2ccd244ed1882420ea321b3594ea90ef0dfda9b089e1c80f2efdb79807f1675f"
        );
    }

    #[test]
    fn writes_trust_state_without_removing_existing_state_fields() {
        let temp = TempDir::new().unwrap();
        let state_config = temp.path().join("config.toml");
        fs::write(
            &state_config,
            r#"[hooks.state."existing:key"]
enabled = false
"#,
        )
        .unwrap();
        let entries = vec![CodexHookTrustEntry {
            key: "existing:key".to_owned(),
            trusted_hash: "sha256:abc".to_owned(),
            event_name: "Stop",
            group_index: 0,
            handler_index: 0,
            command: "echo stop".to_owned(),
        }];

        assert_eq!(write_trust_state(&state_config, &entries).unwrap(), 1);
        let written = fs::read_to_string(&state_config).unwrap();
        assert!(written.contains("enabled = false"));
        assert!(written.contains("trusted_hash = \"sha256:abc\""));
    }
}
