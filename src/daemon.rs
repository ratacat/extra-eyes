use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::conversation::normalize_hook_payload;
use crate::ipc::{self, IpcMessage, Request, Response, PROTOCOL_VERSION};
use crate::paths::ProjectPaths;
use crate::pidfile::{PidFileGuard, PidInfo};
use crate::profiles;
use crate::state::{
    new_conversation_record, new_cursor_record, new_message_record, new_watcher_status_record,
    ConversationRecord, MessageRecord, StateStore,
};
use crate::unix::{is_socket, set_private_file_permissions};
use crate::watcher::{self, WatcherMessage, WatcherStatusEvent};
use crate::{EyesError, Result};

pub fn start_foreground(project: Option<&Path>) -> Result<()> {
    let paths = ProjectPaths::resolve(project)?;
    paths.ensure()?;

    let mut stale_socket = false;
    if paths.socket_path().exists() {
        match ipc::send_request(
            paths.socket_path(),
            &Request::Ping {
                protocol: PROTOCOL_VERSION,
            },
        ) {
            Ok(Response::Pong {
                protocol: PROTOCOL_VERSION,
            }) => return Err(EyesError::AlreadyRunning),
            Ok(Response::Error { code, message, .. }) => {
                return Err(EyesError::Protocol(format!(
                    "existing daemon returned {code}: {message}"
                )))
            }
            Ok(other) => {
                return Err(EyesError::Protocol(format!(
                    "existing socket answered with unexpected response: {other:?}"
                )))
            }
            Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => {
                stale_socket = true;
            }
            Err(error) => return Err(error),
        }
    }

    let pid_info = PidInfo {
        pid: std::process::id(),
        project_root: paths.identity().root_string().to_owned(),
        project_hash: paths.identity().hash().to_owned(),
        started_at_ms: now_ms(),
    };
    let mut pidfile = PidFileGuard::acquire(paths.pid_path(), &pid_info)?;

    if stale_socket {
        if is_socket(paths.socket_path())? {
            fs::remove_file(paths.socket_path())?;
        } else {
            return Err(EyesError::Protocol(format!(
                "refusing to remove non-socket file at {}",
                paths.socket_path().display()
            )));
        }
    }

    let listener = UnixListener::bind(paths.socket_path())?;
    set_private_file_permissions(paths.socket_path())?;
    listener.set_nonblocking(true)?;

    let mut runtime = DaemonRuntime::load(paths.clone())?;
    let shutdown = install_signal_flag()?;
    serve(listener, &paths, &mut runtime, Arc::clone(&shutdown))?;

    match fs::remove_file(paths.socket_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    pidfile.cleanup()?;
    Ok(())
}

pub fn status(project: Option<&Path>) -> Result<Response> {
    let paths = ProjectPaths::resolve(project)?;
    ipc::send_request(
        paths.socket_path(),
        &Request::Status {
            protocol: PROTOCOL_VERSION,
        },
    )
}

pub fn stop(project: Option<&Path>) -> Result<Response> {
    let paths = ProjectPaths::resolve(project)?;
    ipc::send_request(
        paths.socket_path(),
        &Request::Stop {
            protocol: PROTOCOL_VERSION,
        },
    )
}

fn serve(
    listener: UnixListener,
    paths: &ProjectPaths,
    runtime: &mut DaemonRuntime,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false)?;
                handle_client(stream, paths, runtime, Arc::clone(&shutdown))?
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn handle_client(
    mut stream: UnixStream,
    paths: &ProjectPaths,
    runtime: &mut DaemonRuntime,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let request = match ipc::read_frame::<_, Request>(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let _ = ipc::write_frame(
                &mut stream,
                &Response::Error {
                    protocol: PROTOCOL_VERSION,
                    code: "bad_request".to_owned(),
                    message: error.to_string(),
                },
            );
            return Ok(());
        }
    };

    if ipc::request_protocol(&request) != PROTOCOL_VERSION {
        ipc::write_frame(
            &mut stream,
            &Response::Error {
                protocol: PROTOCOL_VERSION,
                code: "unsupported_protocol".to_owned(),
                message: format!(
                    "expected protocol {}, got {}",
                    PROTOCOL_VERSION,
                    ipc::request_protocol(&request)
                ),
            },
        )?;
        return Ok(());
    }

    let response = match request {
        Request::Ping { .. } => Response::Pong {
            protocol: PROTOCOL_VERSION,
        },
        Request::Status { .. } => Response::Status {
            protocol: PROTOCOL_VERSION,
            pid: std::process::id(),
            project_root: paths.identity().root_string().to_owned(),
            project_hash: paths.identity().hash().to_owned(),
            socket_path: paths.socket_path().display().to_string(),
            state_dir: paths.state_dir().display().to_string(),
        },
        Request::Stop { .. } => {
            shutdown.store(true, Ordering::SeqCst);
            Response::Stopping {
                protocol: PROTOCOL_VERSION,
            }
        }
        Request::EnqueueMessage {
            channel, payload, ..
        } => match runtime.enqueue_message(channel, payload) {
            Ok(message_id) => Response::MessageEnqueued {
                protocol: PROTOCOL_VERSION,
                message_id,
            },
            Err(error) => error_response(error),
        },
        Request::FetchMessages {
            channel,
            cursor_key,
            limit,
            ..
        } => match runtime.fetch_messages(channel, cursor_key, limit) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::CommitCursor {
            channel,
            cursor_key,
            through_message_id,
            ..
        } => match runtime.commit_cursor(channel, cursor_key, through_message_id) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::RecordConversation {
            harness,
            event,
            payload,
            ..
        } => match runtime.record_conversation(harness, event, payload) {
            Ok(event_id) => Response::ConversationRecorded {
                protocol: PROTOCOL_VERSION,
                event_id,
            },
            Err(error) => error_response(error),
        },
        Request::RunWatcher {
            profile,
            tick_id,
            context,
            ..
        } => match profiles::resolve_profile(Some(paths.identity().root()), profile.as_deref())
            .and_then(|resolved| {
                watcher::run_profile(&resolved.profile, paths.identity().root(), tick_id, context)
            }) {
            Ok(run) => match runtime.record_watcher_run(run) {
                Ok(response) => response,
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
    };

    ipc::write_frame(&mut stream, &response)?;
    Ok(())
}

#[derive(Debug)]
struct DaemonRuntime {
    store: StateStore,
    messages: Vec<MessageRecord>,
    cursors: BTreeMap<(String, String), u64>,
    conversation: Vec<ConversationRecord>,
    next_message_id: u64,
    next_conversation_id: u64,
    reported_failures: BTreeSet<String>,
}

impl DaemonRuntime {
    fn load(paths: ProjectPaths) -> Result<Self> {
        let store = StateStore::new(paths);
        let replayed = store.replay()?;
        let cursors = replayed
            .cursors
            .into_iter()
            .map(|(key, cursor)| (key, cursor.last_message_id))
            .collect();
        Ok(Self {
            store,
            messages: replayed.messages,
            cursors,
            conversation: replayed.conversation,
            next_message_id: replayed.next_message_id.max(1),
            next_conversation_id: replayed.next_conversation_id.max(1),
            reported_failures: BTreeSet::new(),
        })
    }

    fn enqueue_message(&mut self, channel: String, payload: serde_json::Value) -> Result<u64> {
        validate_name("channel", &channel)?;
        let message_id = self.next_message_id;
        let message = new_message_record(message_id, now_ms(), channel, payload);
        self.store.append_message(&message)?;
        self.messages.push(message);
        self.next_message_id += 1;
        Ok(message_id)
    }

    fn fetch_messages(
        &self,
        channel: String,
        cursor_key: String,
        limit: Option<u32>,
    ) -> Result<Response> {
        validate_name("channel", &channel)?;
        validate_name("cursor_key", &cursor_key)?;
        let limit = limit.unwrap_or(100);
        if limit == 0 {
            return Err(EyesError::Protocol(
                "fetch limit must be greater than zero".to_owned(),
            ));
        }
        if limit > 1000 {
            return Err(EyesError::Protocol(
                "fetch limit must be <= 1000".to_owned(),
            ));
        }
        let last_message_id = self
            .cursors
            .get(&(channel.clone(), cursor_key.clone()))
            .copied()
            .unwrap_or(0);
        let messages = self
            .messages
            .iter()
            .filter(|message| message.channel == channel && message.message_id > last_message_id)
            .take(limit as usize)
            .map(message_to_ipc)
            .collect();

        Ok(Response::Messages {
            protocol: PROTOCOL_VERSION,
            channel,
            cursor_key,
            last_message_id,
            messages,
        })
    }

    fn commit_cursor(
        &mut self,
        channel: String,
        cursor_key: String,
        through_message_id: u64,
    ) -> Result<Response> {
        validate_name("channel", &channel)?;
        validate_name("cursor_key", &cursor_key)?;
        let key = (channel.clone(), cursor_key.clone());
        let current = self.cursors.get(&key).copied().unwrap_or(0);
        if through_message_id <= current {
            return Ok(Response::CursorCommitted {
                protocol: PROTOCOL_VERSION,
                channel,
                cursor_key,
                last_message_id: current,
            });
        }
        if !self
            .messages
            .iter()
            .any(|message| message.channel == channel && message.message_id == through_message_id)
        {
            return Err(EyesError::Protocol(format!(
                "cannot commit cursor through unknown message id {through_message_id} on channel {channel}"
            )));
        }

        let cursor = new_cursor_record(
            channel.clone(),
            cursor_key.clone(),
            through_message_id,
            now_ms(),
        );
        self.store.append_cursor(&cursor)?;
        self.cursors.insert(key, through_message_id);
        Ok(Response::CursorCommitted {
            protocol: PROTOCOL_VERSION,
            channel,
            cursor_key,
            last_message_id: through_message_id,
        })
    }

    fn record_conversation(
        &mut self,
        harness: String,
        event: String,
        payload: serde_json::Value,
    ) -> Result<u64> {
        let normalized = normalize_hook_payload(&harness, &event, payload, now_ms())?;
        let event_id = self.next_conversation_id;
        let record = new_conversation_record(event_id, normalized);
        self.store.append_conversation(&record)?;
        self.conversation.push(record);
        self.next_conversation_id += 1;
        Ok(event_id)
    }

    fn record_watcher_run(&mut self, run: watcher::WatcherRunResult) -> Result<Response> {
        let mut message_ids = Vec::new();
        for message in &run.messages {
            message_ids.push(self.enqueue_watcher_message(message)?);
        }
        if !run.messages.is_empty()
            && !run
                .statuses
                .iter()
                .any(|status| status.outcome == "api_failure")
        {
            self.clear_reported_failures_for(&run.watcher);
        }
        for status in &run.statuses {
            self.record_watcher_status(status)?;
            if let Some(message_id) = self.maybe_enqueue_status_diagnostic(status)? {
                message_ids.push(message_id);
            }
        }
        Ok(Response::WatcherRun {
            protocol: PROTOCOL_VERSION,
            watcher: run.watcher,
            tick_id: run.tick_id,
            message_ids,
            statuses: run.statuses,
        })
    }

    fn enqueue_watcher_message(&mut self, message: &WatcherMessage) -> Result<u64> {
        let payload = serde_json::json!({
            "watcher": message.watcher,
            "tick_id": message.tick_id,
            "severity": message.severity,
            "refs": message.refs,
            "text": message.text,
            "usage": message.usage,
        });
        self.enqueue_message("hook".to_owned(), payload)
    }

    fn record_watcher_status(&mut self, status: &WatcherStatusEvent) -> Result<()> {
        let record = new_watcher_status_record(
            status.watcher.clone(),
            status.outcome.clone(),
            now_ms(),
            Some(status.tick_id.clone()),
            Some(status.text.clone()),
            Some(status.details.clone()),
        );
        self.store.append_watcher_status(&record)
    }

    fn maybe_enqueue_status_diagnostic(
        &mut self,
        status: &WatcherStatusEvent,
    ) -> Result<Option<u64>> {
        if status.outcome != "api_failure" {
            return Ok(None);
        }
        let key = format!("{}:{}:{}", status.watcher, status.outcome, status.text);
        if !self.reported_failures.insert(key) {
            return Ok(None);
        }
        let payload = serde_json::json!({
            "watcher": status.watcher,
            "tick_id": status.tick_id,
            "severity": "error",
            "refs": [],
            "text": format!(
                "Watcher `{}` reported API failure: {}. Further identical failures are suppressed until recovery.",
                status.watcher, status.text
            ),
            "usage": null,
        });
        self.enqueue_message("hook".to_owned(), payload).map(Some)
    }

    fn clear_reported_failures_for(&mut self, watcher: &str) {
        let prefix = format!("{watcher}:");
        self.reported_failures
            .retain(|key| !key.starts_with(&prefix));
    }
}

fn message_to_ipc(message: &MessageRecord) -> IpcMessage {
    IpcMessage {
        message_id: message.message_id,
        channel: message.channel.clone(),
        created_at_ms: message.created_at_ms,
        payload: message.payload.clone(),
    }
}

fn validate_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(EyesError::Protocol(format!("{label} cannot be empty")));
    }
    Ok(())
}

fn error_response(error: EyesError) -> Response {
    Response::Error {
        protocol: PROTOCOL_VERSION,
        code: "request_failed".to_owned(),
        message: error.to_string(),
    }
}

fn install_signal_flag() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;
    Ok(shutdown)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_millis() as u64
}
