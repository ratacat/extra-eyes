use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::ConversationEvent;
use crate::paths::ProjectPaths;
use crate::{EyesError, Result};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageRecord {
    pub v: u32,
    pub message_id: u64,
    pub channel: String,
    pub created_at_ms: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CursorRecord {
    pub v: u32,
    pub channel: String,
    pub cursor_key: String,
    pub last_message_id: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub v: u32,
    pub session_id: String,
    pub harness: String,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationRecord {
    pub v: u32,
    pub event_id: u64,
    pub harness: String,
    pub event: String,
    pub role: String,
    pub text: String,
    pub source: String,
    pub session_id: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatcherStatusRecord {
    pub v: u32,
    pub watcher: String,
    pub status: String,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub tick_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub details: Option<Value>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayedState {
    pub messages: Vec<MessageRecord>,
    pub cursors: BTreeMap<(String, String), CursorRecord>,
    pub sessions: BTreeMap<String, SessionRecord>,
    pub conversation: Vec<ConversationRecord>,
    pub watcher_statuses: BTreeMap<String, WatcherStatusRecord>,
    pub next_message_id: u64,
    pub next_conversation_id: u64,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    paths: ProjectPaths,
}

impl StateStore {
    pub fn new(paths: ProjectPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &ProjectPaths {
        &self.paths
    }

    pub fn replay(&self) -> Result<ReplayedState> {
        let messages = replay_jsonl::<MessageRecord>(self.paths.messages_path())?;
        let cursor_rows = replay_jsonl::<CursorRecord>(self.paths.cursors_path())?;
        let session_rows = replay_jsonl::<SessionRecord>(self.paths.sessions_path())?;
        let conversation = replay_jsonl::<ConversationRecord>(self.paths.conversation_path())?;
        let watcher_status_rows =
            replay_jsonl::<WatcherStatusRecord>(self.paths.watcher_status_path())?;

        for message in &messages {
            assert_state_version(message.v, "message")?;
        }
        for cursor in &cursor_rows {
            assert_state_version(cursor.v, "cursor")?;
        }
        for session in &session_rows {
            assert_state_version(session.v, "session")?;
        }
        for event in &conversation {
            assert_state_version(event.v, "conversation")?;
        }
        for status in &watcher_status_rows {
            assert_state_version(status.v, "watcher status")?;
        }

        let next_message_id = messages
            .iter()
            .map(|message| message.message_id)
            .max()
            .unwrap_or(0)
            + 1;
        let next_conversation_id = conversation
            .iter()
            .map(|event| event.event_id)
            .max()
            .unwrap_or(0)
            + 1;
        let cursors = cursor_rows
            .into_iter()
            .map(|cursor| ((cursor.channel.clone(), cursor.cursor_key.clone()), cursor))
            .collect();
        let sessions = session_rows
            .into_iter()
            .map(|session| (session.session_id.clone(), session))
            .collect();
        let watcher_statuses = watcher_status_rows
            .into_iter()
            .map(|status| (watcher_status_key(&status), status))
            .collect();

        Ok(ReplayedState {
            messages,
            cursors,
            sessions,
            conversation,
            watcher_statuses,
            next_message_id,
            next_conversation_id,
        })
    }

    pub fn last_conversation_id(&self) -> Result<u64> {
        let Some(row) = self.last_conversation()? else {
            return Ok(0);
        };
        Ok(row.event_id)
    }

    pub fn last_conversation(&self) -> Result<Option<ConversationRecord>> {
        let Some(row) = replay_last_jsonl::<ConversationRecord>(self.paths.conversation_path())?
        else {
            return Ok(None);
        };
        assert_state_version(row.v, "conversation")?;
        Ok(Some(row))
    }

    pub fn conversations_after(&self, event_id: u64) -> Result<Vec<ConversationRecord>> {
        let rows = replay_jsonl::<ConversationRecord>(self.paths.conversation_path())?;
        let mut out = Vec::new();
        for row in rows {
            assert_state_version(row.v, "conversation")?;
            if row.event_id > event_id {
                out.push(row);
            }
        }
        Ok(out)
    }

    pub fn append_message(&self, message: &MessageRecord) -> Result<()> {
        assert_state_version(message.v, "message")?;
        append_jsonl(self.paths.messages_path(), message)
    }

    pub fn append_cursor(&self, cursor: &CursorRecord) -> Result<()> {
        assert_state_version(cursor.v, "cursor")?;
        append_jsonl(self.paths.cursors_path(), cursor)
    }

    pub fn append_session(&self, session: &SessionRecord) -> Result<()> {
        assert_state_version(session.v, "session")?;
        append_jsonl(self.paths.sessions_path(), session)
    }

    pub fn append_conversation(&self, event: &ConversationRecord) -> Result<()> {
        assert_state_version(event.v, "conversation")?;
        append_jsonl(self.paths.conversation_path(), event)
    }

    pub fn append_watcher_status(&self, status: &WatcherStatusRecord) -> Result<()> {
        assert_state_version(status.v, "watcher status")?;
        append_jsonl(self.paths.watcher_status_path(), status)
    }
}

pub fn new_message_record(
    message_id: u64,
    created_at_ms: u64,
    channel: impl Into<String>,
    payload: Value,
) -> MessageRecord {
    MessageRecord {
        v: STATE_VERSION,
        message_id,
        channel: channel.into(),
        created_at_ms,
        payload,
    }
}

pub fn new_cursor_record(
    channel: impl Into<String>,
    cursor_key: impl Into<String>,
    last_message_id: u64,
    updated_at_ms: u64,
) -> CursorRecord {
    CursorRecord {
        v: STATE_VERSION,
        channel: channel.into(),
        cursor_key: cursor_key.into(),
        last_message_id,
        updated_at_ms,
    }
}

pub fn new_conversation_record(event_id: u64, event: ConversationEvent) -> ConversationRecord {
    ConversationRecord {
        v: STATE_VERSION,
        event_id,
        harness: event.harness,
        event: event.event,
        role: event.role,
        text: event.text,
        source: event.source,
        session_id: event.session_id,
        timestamp_ms: event.timestamp_ms,
    }
}

impl ConversationRecord {
    pub fn to_context_event(&self) -> ConversationEvent {
        ConversationEvent {
            harness: self.harness.clone(),
            event: self.event.clone(),
            role: self.role.clone(),
            text: self.text.clone(),
            source: self.source.clone(),
            session_id: self.session_id.clone(),
            timestamp_ms: self.timestamp_ms,
        }
    }
}

pub fn new_watcher_status_record(
    watcher: impl Into<String>,
    status: impl Into<String>,
    updated_at_ms: u64,
    tick_id: Option<String>,
    message: Option<String>,
    details: Option<Value>,
) -> WatcherStatusRecord {
    WatcherStatusRecord {
        v: STATE_VERSION,
        watcher: watcher.into(),
        status: status.into(),
        updated_at_ms,
        tick_id,
        message,
        details,
    }
}

pub fn watcher_status_key(status: &WatcherStatusRecord) -> String {
    let target_harness = status
        .details
        .as_ref()
        .and_then(|details| details.get("target_harness"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let target_session_id = status
        .details
        .as_ref()
        .and_then(|details| details.get("target_session_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "{}\t{}\t{}",
        status.watcher, target_harness, target_session_id
    )
}

fn append_jsonl<T>(path: &Path, row: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, row)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn replay_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let has_trailing_newline = bytes.ends_with(b"\n");
    let parse_bytes = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.as_slice(),
        Err(error) if !has_trailing_newline => {
            let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
                eprintln!(
                    "warning: ignoring malformed final jsonl row in {}: {error}",
                    path.display()
                );
                return Ok(Vec::new());
            };
            &bytes[..=last_newline]
        }
        Err(_) => {
            return Err(EyesError::State(format!(
                "{} is not valid UTF-8",
                path.display()
            )))
        }
    };
    let text = std::str::from_utf8(parse_bytes)
        .map_err(|_| EyesError::State(format!("{} is not valid UTF-8", path.display())))?;
    let mut rows = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let is_final_line = index + 1 == lines.len();
        match serde_json::from_str::<T>(line) {
            Ok(row) => rows.push(row),
            Err(error) if is_final_line && !has_trailing_newline => {
                eprintln!(
                    "warning: ignoring malformed final jsonl row in {}: {error}",
                    path.display()
                );
            }
            Err(error) => {
                return Err(EyesError::State(format!(
                    "malformed jsonl row {} in {}: {error}",
                    index + 1,
                    path.display()
                )));
            }
        }
    }
    Ok(rows)
}

fn replay_last_jsonl<T>(path: &Path) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if bytes.is_empty() {
        return Ok(None);
    }

    let has_trailing_newline = bytes.ends_with(b"\n");
    let parse_bytes = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.as_slice(),
        Err(error) if !has_trailing_newline => {
            let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
                eprintln!(
                    "warning: ignoring malformed final jsonl row in {}: {error}",
                    path.display()
                );
                return Ok(None);
            };
            &bytes[..=last_newline]
        }
        Err(_) => {
            return Err(EyesError::State(format!(
                "{} is not valid UTF-8",
                path.display()
            )))
        }
    };
    let text = std::str::from_utf8(parse_bytes)
        .map_err(|_| EyesError::State(format!("{} is not valid UTF-8", path.display())))?;
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let is_final_line = index + 1 == lines.len();
        match serde_json::from_str::<T>(line) {
            Ok(row) => return Ok(Some(row)),
            Err(error) if is_final_line && !has_trailing_newline => {
                eprintln!(
                    "warning: ignoring malformed final jsonl row in {}: {error}",
                    path.display()
                );
                continue;
            }
            Err(error) => {
                return Err(EyesError::State(format!(
                    "malformed jsonl row {} in {}: {error}",
                    index + 1,
                    path.display()
                )));
            }
        }
    }
    Ok(None)
}

fn assert_state_version(version: u32, label: &str) -> Result<()> {
    if version == STATE_VERSION {
        Ok(())
    } else {
        Err(EyesError::State(format!(
            "unsupported {label} state version {version}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::identity::ProjectIdentity;
    use crate::paths::ProjectPaths;

    use super::*;

    fn store(temp: &TempDir) -> StateStore {
        let runtime = temp.path().join("runtime");
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let identity = ProjectIdentity::from_root(project).unwrap();
        let paths = ProjectPaths::from_identity_with_runtime_base(identity, runtime).unwrap();
        paths.ensure().unwrap();
        StateStore::new(paths)
    }

    #[test]
    fn replays_messages_and_last_cursor_values() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);

        store
            .append_message(&new_message_record(
                1,
                10,
                "watcher",
                serde_json::json!({"watcher":"security","severity":"info","body":"check auth path"}),
            ))
            .unwrap();
        store
            .append_cursor(&new_cursor_record("watcher", "codex:session:hook", 1, 20))
            .unwrap();
        store
            .append_cursor(&new_cursor_record("watcher", "codex:session:hook", 2, 30))
            .unwrap();

        let replayed = store.replay().unwrap();

        assert_eq!(replayed.next_message_id, 2);
        assert_eq!(replayed.messages.len(), 1);
        assert_eq!(
            replayed
                .cursors
                .get(&("watcher".to_owned(), "codex:session:hook".to_owned()))
                .unwrap()
                .last_message_id,
            2
        );
    }

    #[test]
    fn ignores_malformed_final_partial_line() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        store
            .append_message(&new_message_record(
                1,
                10,
                "watcher",
                serde_json::json!({"body":"ok"}),
            ))
            .unwrap();
        fs::write(
            store.paths.messages_path(),
            "{\"v\":1,\"message_id\":1,\"channel\":\"watcher\",\"created_at_ms\":10,\"payload\":{\"body\":\"ok\"}}\n{\"v\"",
        )
        .unwrap();

        let replayed = store.replay().unwrap();

        assert_eq!(replayed.messages.len(), 1);
    }

    #[test]
    fn last_conversation_id_ignores_malformed_final_partial_line() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);

        store
            .append_conversation(&new_conversation_record(
                1,
                ConversationEvent {
                    harness: "codex".to_owned(),
                    event: "userpromptsubmit".to_owned(),
                    role: "user".to_owned(),
                    text: "first".to_owned(),
                    source: "userpromptsubmit".to_owned(),
                    session_id: "s1".to_owned(),
                    timestamp_ms: 10,
                },
            ))
            .unwrap();
        store
            .append_conversation(&new_conversation_record(
                2,
                ConversationEvent {
                    harness: "codex".to_owned(),
                    event: "stop".to_owned(),
                    role: "assistant".to_owned(),
                    text: "second".to_owned(),
                    source: "stop".to_owned(),
                    session_id: "s1".to_owned(),
                    timestamp_ms: 20,
                },
            ))
            .unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.paths().conversation_path())
            .unwrap();
        file.write_all(br#"{"v":1,"event_id":"#).unwrap();

        assert_eq!(store.last_conversation_id().unwrap(), 2);
    }

    #[test]
    fn last_conversation_id_ignores_final_partial_utf8() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);

        store
            .append_conversation(&new_conversation_record(
                1,
                ConversationEvent {
                    harness: "codex".to_owned(),
                    event: "userpromptsubmit".to_owned(),
                    role: "user".to_owned(),
                    text: "first".to_owned(),
                    source: "userpromptsubmit".to_owned(),
                    session_id: "s1".to_owned(),
                    timestamp_ms: 10,
                },
            ))
            .unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.paths().conversation_path())
            .unwrap();
        file.write_all(b"{\"v\":1,\"event_id\":2,\"text\":\"\xF0\x9F")
            .unwrap();

        assert_eq!(store.last_conversation_id().unwrap(), 1);
    }
}
