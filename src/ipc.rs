use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::watcher::{WatcherContext, WatcherMessage, WatcherStatusEvent};
use crate::{EyesError, Result};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping {
        protocol: u32,
    },
    Status {
        protocol: u32,
    },
    Stop {
        protocol: u32,
    },
    EnqueueMessage {
        protocol: u32,
        channel: String,
        payload: Value,
    },
    FetchMessages {
        protocol: u32,
        channel: String,
        cursor_key: String,
        limit: Option<u32>,
        after_message_id: Option<u64>,
        targeted_only: bool,
        include_all_targets: bool,
    },
    CommitCursor {
        protocol: u32,
        channel: String,
        cursor_key: String,
        through_message_id: u64,
    },
    CommitCursorIfCurrent {
        protocol: u32,
        channel: String,
        cursor_key: String,
        expected_last_message_id: u64,
        through_message_id: u64,
    },
    WatcherStatuses {
        protocol: u32,
    },
    EnsureWatcherCheckIn {
        protocol: u32,
        watcher: String,
        target_session_id: Option<String>,
    },
    RecordConversation {
        protocol: u32,
        harness: String,
        event: String,
        payload: Value,
    },
    RunWatcher {
        protocol: u32,
        profile: Option<String>,
        tick_id: String,
        context: WatcherContext,
        target_session_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcMessage {
    pub message_id: u64,
    pub channel: String,
    pub created_at_ms: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpcWatcherStatus {
    pub watcher: String,
    pub status: String,
    pub severity: String,
    pub text: String,
    pub tick_id: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatcherRunSummary {
    pub state: String,
    pub severity: String,
    pub text: String,
    pub message_count: usize,
    pub status_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong {
        protocol: u32,
    },
    Status {
        protocol: u32,
        pid: u32,
        project_root: String,
        project_hash: String,
        socket_path: String,
        state_dir: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        build_id: Option<String>,
    },
    Stopping {
        protocol: u32,
    },
    MessageEnqueued {
        protocol: u32,
        message_id: u64,
    },
    Messages {
        protocol: u32,
        channel: String,
        cursor_key: String,
        last_message_id: u64,
        latest_message_id: u64,
        messages: Vec<IpcMessage>,
    },
    CursorCommitted {
        protocol: u32,
        channel: String,
        cursor_key: String,
        last_message_id: u64,
    },
    CursorCommitStale {
        protocol: u32,
        channel: String,
        cursor_key: String,
        last_message_id: u64,
    },
    ConversationRecorded {
        protocol: u32,
        event_id: u64,
    },
    WatcherCheckIn {
        protocol: u32,
        watcher: String,
        message_id: Option<u64>,
    },
    WatcherRun {
        protocol: u32,
        watcher: String,
        tick_id: String,
        message_ids: Vec<u64>,
        messages: Vec<WatcherMessage>,
        statuses: Vec<WatcherStatusEvent>,
        summary: WatcherRunSummary,
    },
    WatcherStatuses {
        protocol: u32,
        watchers: Vec<IpcWatcherStatus>,
    },
    Error {
        protocol: u32,
        code: String,
        message: String,
    },
}

pub fn send_request(socket_path: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound
            || error.kind() == std::io::ErrorKind::ConnectionRefused
        {
            EyesError::NotRunning
        } else {
            EyesError::Io(error)
        }
    })?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

pub fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: Write,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() {
        return Err(EyesError::Protocol(
            "refusing to write empty frame".to_owned(),
        ));
    }
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(EyesError::Protocol(format!(
            "frame exceeds {MAX_FRAME_SIZE} byte maximum"
        )));
    }
    writer.write_all(&(bytes.len() as u64).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut length_bytes = [0_u8; 8];
    reader.read_exact(&mut length_bytes)?;
    let length = u64::from_be_bytes(length_bytes);
    if length == 0 {
        return Err(EyesError::Protocol("received empty frame".to_owned()));
    }
    if length > MAX_FRAME_SIZE as u64 {
        return Err(EyesError::Protocol(format!(
            "received frame exceeds {MAX_FRAME_SIZE} byte maximum"
        )));
    }
    let mut bytes = vec![0_u8; length as usize];
    reader.read_exact(&mut bytes)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| EyesError::Protocol("received non-UTF-8 frame".to_owned()))?;
    Ok(serde_json::from_str(text)?)
}

pub fn request_protocol(request: &Request) -> u32 {
    match request {
        Request::Ping { protocol }
        | Request::Status { protocol }
        | Request::Stop { protocol }
        | Request::EnqueueMessage { protocol, .. }
        | Request::FetchMessages { protocol, .. }
        | Request::CommitCursor { protocol, .. }
        | Request::CommitCursorIfCurrent { protocol, .. }
        | Request::WatcherStatuses { protocol }
        | Request::EnsureWatcherCheckIn { protocol, .. }
        | Request::RecordConversation { protocol, .. }
        | Request::RunWatcher { protocol, .. } => *protocol,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn round_trips_a_length_prefixed_json_frame() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Request::Ping { protocol: 1 }).unwrap();

        let decoded: Request = read_frame(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(decoded, Request::Ping { protocol: 1 });
    }

    #[test]
    fn rejects_zero_length_frames() {
        let mut bytes = Cursor::new(0_u64.to_be_bytes().to_vec());
        let result: Result<Request> = read_frame(&mut bytes);

        assert!(matches!(result, Err(EyesError::Protocol(_))));
    }

    #[test]
    fn status_response_accepts_missing_build_metadata_from_old_daemons() {
        let response: Response = serde_json::from_value(serde_json::json!({
            "type": "status",
            "protocol": 1,
            "pid": 42,
            "project_root": "/tmp/project",
            "project_hash": "hash",
            "socket_path": "/tmp/eyes.sock",
            "state_dir": "/tmp/project/.eyes/state"
        }))
        .unwrap();

        assert!(matches!(
            response,
            Response::Status {
                version: None,
                build_id: None,
                ..
            }
        ));
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut bytes = Cursor::new(((MAX_FRAME_SIZE as u64) + 1).to_be_bytes().to_vec());
        let result: Result<Request> = read_frame(&mut bytes);

        assert!(matches!(result, Err(EyesError::Protocol(_))));
    }

    #[test]
    fn writes_and_reads_a_boundary_sized_frame() {
        let value = "x".repeat(MAX_FRAME_SIZE - 2);
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &value).unwrap();

        let decoded: String = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn refuses_to_write_oversized_frames() {
        let value = "x".repeat(MAX_FRAME_SIZE - 1);
        let mut bytes = Vec::new();

        let result = write_frame(&mut bytes, &value);

        assert!(matches!(result, Err(EyesError::Protocol(_))));
    }
}
