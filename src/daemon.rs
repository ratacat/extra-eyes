use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::build_info;
use crate::conversation::normalize_hook_payload;
use crate::ipc::{
    self, IpcMessage, IpcWatchStatus, IpcWatcherStatus, Request, Response, WatcherRunSummary,
    PROTOCOL_VERSION,
};
use crate::paths::ProjectPaths;
use crate::pidfile::{PidFileGuard, PidInfo};
use crate::profiles;
use crate::routing::RouteKey;
use crate::state::{
    new_conversation_record, new_cursor_record, new_message_record, new_watcher_status_record,
    watcher_status_key, ConversationRecord, MessageRecord, StateStore, WatcherStatusRecord,
};
use crate::unix::{is_socket, set_private_file_permissions};
use crate::watcher::{self, WatcherMessage, WatcherStatusEvent};
use crate::{EyesError, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct DetachedStart {
    pub pid: u32,
    pub project_root: String,
    pub project_hash: String,
    pub socket_path: String,
    pub state_dir: String,
    pub version: String,
    pub build_id: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Restarted {
    pub stopped_existing: bool,
    #[serde(flatten)]
    pub started: DetachedStart,
}

pub fn start_detached(project: Option<&Path>) -> Result<DetachedStart> {
    let paths = ProjectPaths::resolve(project)?;
    paths.ensure()?;

    match daemon_status_response(&paths) {
        Ok(Response::Status { .. }) => return Err(EyesError::AlreadyRunning),
        Ok(other) => {
            return Err(EyesError::Protocol(format!(
                "existing daemon returned unexpected status response: {other:?}"
            )))
        }
        Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => {}
        Err(error) => return Err(error),
    }

    let log_path = paths.runtime_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    set_private_file_permissions(&log_path)?;

    let mut command = Command::new(eyes_executable()?);
    command
        .args(["daemon", "foreground", "--project"])
        .arg(paths.identity().root())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                terminate_detached_child(&mut child);
                return Err(error.into());
            }
        } {
            return Err(EyesError::Protocol(format!(
                "eyes daemon exited before it became ready with status {status}; see {}",
                log_path.display()
            )));
        }

        match daemon_status_response(&paths) {
            Ok(Response::Status {
                pid,
                project_root,
                project_hash,
                socket_path,
                state_dir,
                version,
                build_id,
                ..
            }) => {
                if match child.try_wait() {
                    Ok(status) => status,
                    Err(error) => {
                        terminate_detached_child(&mut child);
                        return Err(error.into());
                    }
                }
                .is_some()
                {
                    return Err(EyesError::AlreadyRunning);
                }
                return Ok(DetachedStart {
                    pid,
                    project_root,
                    project_hash,
                    socket_path,
                    state_dir,
                    version: version.unwrap_or_else(|| "unknown".to_owned()),
                    build_id: build_id.unwrap_or_else(|| "unknown".to_owned()),
                    log_path,
                });
            }
            Ok(other) => {
                terminate_detached_child(&mut child);
                return Err(EyesError::Protocol(format!(
                    "daemon returned unexpected status response after start: {other:?}"
                )));
            }
            Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                terminate_detached_child(&mut child);
                return Err(error);
            }
        }
    }

    terminate_detached_child(&mut child);
    Err(EyesError::Protocol(format!(
        "eyes daemon did not become ready within 5s; see {}",
        log_path.display()
    )))
}

fn terminate_detached_child(child: &mut Child) {
    let process_group = child.id() as libc::pid_t;
    let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

fn eyes_executable() -> Result<PathBuf> {
    let current = env::current_exe()?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "eyes" || name == "eyes.exe")
    {
        return Ok(current);
    }

    for name in ["eyes", "eyes.exe"] {
        let candidate = current.with_file_name(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(EyesError::Config(format!(
        "could not locate eyes next to {}",
        current.display()
    )))
}

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

    let runtime = Arc::new(Mutex::new(DaemonRuntimes::load(paths.clone())?));
    let shutdown = install_signal_flag()?;
    serve(
        listener,
        &paths,
        Arc::clone(&runtime),
        Arc::clone(&shutdown),
    )?;
    watcher::kill_active_process_groups();

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
    daemon_status_response(&paths)
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

pub fn stop_watch(project: Option<&Path>) -> Result<Response> {
    let paths = ProjectPaths::resolve(project)?;
    ipc::send_request(
        paths.socket_path(),
        &Request::StopWatch {
            protocol: PROTOCOL_VERSION,
        },
    )
}

pub fn restart(project: Option<&Path>) -> Result<Restarted> {
    let paths = ProjectPaths::resolve(project)?;
    let stopped_existing = match daemon_status_response(&paths) {
        Ok(Response::Status { .. }) => {
            match ipc::send_request(
                paths.socket_path(),
                &Request::Stop {
                    protocol: PROTOCOL_VERSION,
                },
            )? {
                Response::Stopping { .. } => {}
                Response::Error { code, message, .. } => {
                    return Err(EyesError::Protocol(format!("{code}: {message}")))
                }
                other => {
                    return Err(EyesError::Protocol(format!(
                        "unexpected stop response during restart: {other:?}"
                    )))
                }
            }
            wait_until_not_running(&paths, Duration::from_secs(5))?;
            true
        }
        Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => false,
        Ok(other) => {
            return Err(EyesError::Protocol(format!(
                "daemon returned unexpected status response during restart: {other:?}"
            )))
        }
        Err(error) => return Err(error),
    };

    let started = start_detached(Some(paths.identity().root()))?;
    Ok(Restarted {
        stopped_existing,
        started,
    })
}

fn wait_until_not_running(paths: &ProjectPaths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match daemon_status_response(paths) {
            Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => return Ok(()),
            Ok(Response::Status { .. }) => thread::sleep(Duration::from_millis(50)),
            Ok(other) => {
                return Err(EyesError::Protocol(format!(
                    "daemon returned unexpected status response while stopping: {other:?}"
                )))
            }
            Err(error) => return Err(error),
        }
    }

    Err(EyesError::Protocol(format!(
        "eyes daemon did not stop within {} ms",
        timeout.as_millis()
    )))
}

fn serve(
    listener: UnixListener,
    paths: &ProjectPaths,
    runtime: Arc<Mutex<DaemonRuntimes>>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false)?;
                let paths = paths.clone();
                let runtime = Arc::clone(&runtime);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, &paths, runtime, shutdown) {
                        eprintln!("eyes daemon client error: {error}");
                    }
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn daemon_status_response(paths: &ProjectPaths) -> Result<Response> {
    let request = ipc::project_request(
        paths.identity().root(),
        Request::Status {
            protocol: PROTOCOL_VERSION,
        },
    )?;
    ipc::send_request(paths.socket_path(), &request)
}

fn handle_client(
    mut stream: UnixStream,
    default_paths: &ProjectPaths,
    runtime: Arc<Mutex<DaemonRuntimes>>,
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

    let (paths, request) = match request {
        Request::Project {
            project_root,
            request,
            ..
        } => {
            if matches!(*request, Request::Project { .. }) {
                ipc::write_frame(
                    &mut stream,
                    &Response::Error {
                        protocol: PROTOCOL_VERSION,
                        code: "bad_request".to_owned(),
                        message: "nested project request is not supported".to_owned(),
                    },
                )?;
                return Ok(());
            }
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
            (
                ProjectPaths::resolve(Some(Path::new(&project_root)))?,
                *request,
            )
        }
        request => (default_paths.clone(), request),
    };

    let response = match request {
        Request::Ping { .. } => Response::Pong {
            protocol: PROTOCOL_VERSION,
        },
        Request::Status { .. } => match project_status(&runtime, paths.clone()) {
            Ok((watch, loaded_projects)) => Response::Status {
                protocol: PROTOCOL_VERSION,
                pid: std::process::id(),
                project_root: paths.identity().root_string().to_owned(),
                project_hash: paths.identity().hash().to_owned(),
                socket_path: paths.socket_path().display().to_string(),
                state_dir: paths.state_dir().display().to_string(),
                version: Some(build_info::VERSION.to_owned()),
                build_id: Some(build_info::BUILD_ID.to_owned()),
                watch,
                loaded_projects,
            },
            Err(error) => error_response(error),
        },
        Request::Stop { .. } => {
            shutdown.store(true, Ordering::SeqCst);
            Response::Stopping {
                protocol: PROTOCOL_VERSION,
            }
        }
        Request::EnqueueMessage {
            channel, payload, ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.enqueue_message(channel, payload)
        }) {
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
            after_message_id,
            targeted_only,
            include_all_targets,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.fetch_messages(
                channel,
                cursor_key,
                limit,
                after_message_id,
                targeted_only,
                include_all_targets,
            )
        }) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::CommitCursor {
            channel,
            cursor_key,
            through_message_id,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.commit_cursor(channel, cursor_key, through_message_id)
        }) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::CommitCursorIfCurrent {
            channel,
            cursor_key,
            expected_last_message_id,
            through_message_id,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.commit_cursor_if_current(
                channel,
                cursor_key,
                expected_last_message_id,
                through_message_id,
            )
        }) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::WatcherStatuses { .. } => {
            match with_project_runtime(&runtime, paths.clone(), |runtime| {
                Ok(runtime.watcher_statuses_response())
            }) {
                Ok(response) => response,
                Err(error) => error_response(error),
            }
        }
        Request::WatchHeartbeat {
            active,
            profiles,
            pid,
            tick,
            stale_after_ms,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            Ok(runtime.record_watch_heartbeat(active, profiles, pid, tick, stale_after_ms))
        }) {
            Ok(watch) => Response::WatchHeartbeatRecorded {
                protocol: PROTOCOL_VERSION,
                watch,
            },
            Err(error) => error_response(error),
        },
        Request::StopWatch { .. } => {
            match with_project_runtime(&runtime, paths.clone(), |runtime| runtime.stop_watch()) {
                Ok((stopped, watch)) => Response::WatchStopped {
                    protocol: PROTOCOL_VERSION,
                    stopped,
                    pid: watch.pid,
                    profiles: watch.profiles,
                },
                Err(error) => error_response(error),
            }
        }
        Request::EnsureWatcherCheckIn {
            watcher,
            target_harness,
            target_session_id,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.ensure_watcher_check_in(watcher, target_harness, target_session_id)
        }) {
            Ok(response) => response,
            Err(error) => error_response(error),
        },
        Request::RecordConversation {
            harness,
            event,
            payload,
            ..
        } => match with_project_runtime(&runtime, paths.clone(), |runtime| {
            runtime.record_conversation(harness, event, payload)
        }) {
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
            target_harness,
            target_session_id,
            source_event_id,
            ..
        } => match profiles::resolve_profile(Some(paths.identity().root()), profile.as_deref())
            .and_then(|resolved| {
                watcher::run_profile(&resolved.profile, paths.identity().root(), tick_id, context)
            }) {
            Ok(run) => match with_project_runtime(&runtime, paths.clone(), |runtime| {
                runtime.record_watcher_run(run, target_harness, target_session_id, source_event_id)
            }) {
                Ok(response) => response,
                Err(error) => error_response(error),
            },
            Err(error) => error_response(error),
        },
        Request::Project { .. } => error_response(EyesError::Protocol(
            "nested project request is not supported".to_owned(),
        )),
    };

    ipc::write_frame(&mut stream, &response)?;
    Ok(())
}

fn lock_runtime(runtime: &Arc<Mutex<DaemonRuntimes>>) -> Result<MutexGuard<'_, DaemonRuntimes>> {
    runtime
        .lock()
        .map_err(|_| EyesError::Protocol("daemon runtime lock was poisoned".to_owned()))
}

fn with_project_runtime<T>(
    runtime: &Arc<Mutex<DaemonRuntimes>>,
    paths: ProjectPaths,
    f: impl FnOnce(&mut DaemonRuntime) -> Result<T>,
) -> Result<T> {
    let project_runtime = {
        let mut runtimes = lock_runtime(runtime)?;
        runtimes.for_project(paths)?
    };
    let mut runtime = project_runtime
        .lock()
        .map_err(|_| EyesError::Protocol("project runtime lock was poisoned".to_owned()))?;
    f(&mut runtime)
}

fn project_status(
    runtime: &Arc<Mutex<DaemonRuntimes>>,
    paths: ProjectPaths,
) -> Result<(IpcWatchStatus, Vec<String>)> {
    let (project_runtime, loaded_projects) = {
        let mut runtimes = lock_runtime(runtime)?;
        let project_runtime = runtimes.for_project(paths)?;
        let loaded_projects = runtimes.loaded_projects();
        (project_runtime, loaded_projects)
    };
    let runtime = project_runtime
        .lock()
        .map_err(|_| EyesError::Protocol("project runtime lock was poisoned".to_owned()))?;
    Ok((runtime.watch_status(), loaded_projects))
}

#[derive(Debug)]
struct DaemonRuntimes {
    runtimes: BTreeMap<String, Arc<Mutex<DaemonRuntime>>>,
}

impl DaemonRuntimes {
    fn load(default_paths: ProjectPaths) -> Result<Self> {
        let key = default_paths.identity().root_string().to_owned();
        let runtime = DaemonRuntime::load(default_paths)?;
        let mut runtimes = BTreeMap::new();
        runtimes.insert(key, Arc::new(Mutex::new(runtime)));
        Ok(Self { runtimes })
    }

    fn for_project(&mut self, paths: ProjectPaths) -> Result<Arc<Mutex<DaemonRuntime>>> {
        let key = paths.identity().root_string().to_owned();
        self.evict_missing_except(&key);
        if !self.runtimes.contains_key(&key) {
            paths.ensure()?;
            let runtime = DaemonRuntime::load(paths)?;
            self.runtimes
                .insert(key.clone(), Arc::new(Mutex::new(runtime)));
        }
        self.runtimes
            .get(&key)
            .cloned()
            .ok_or_else(|| EyesError::Protocol(format!("project runtime {key} was not loaded")))
    }

    fn evict_missing_except(&mut self, keep: &str) {
        self.runtimes
            .retain(|root, _| root == keep || Path::new(root).exists());
    }

    fn loaded_projects(&self) -> Vec<String> {
        self.runtimes.keys().cloned().collect()
    }
}

#[derive(Debug)]
struct DaemonRuntime {
    store: StateStore,
    messages: Vec<MessageRecord>,
    cursors: BTreeMap<(String, String), u64>,
    conversation: Vec<ConversationRecord>,
    watcher_statuses: BTreeMap<String, WatcherStatusRecord>,
    watch: IpcWatchStatus,
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
        let mut runtime = Self {
            store,
            messages: replayed.messages,
            cursors,
            conversation: replayed.conversation,
            watcher_statuses: replayed.watcher_statuses,
            watch: IpcWatchStatus::default(),
            next_message_id: replayed.next_message_id.max(1),
            next_conversation_id: replayed.next_conversation_id.max(1),
            reported_failures: BTreeSet::new(),
        };
        runtime.refresh_inbox_mirror()?;
        Ok(runtime)
    }

    fn enqueue_message(&mut self, channel: String, payload: serde_json::Value) -> Result<u64> {
        validate_name("channel", &channel)?;
        let mirror_to_inbox = channel == "hook";
        let message_id = self.next_message_id;
        let message = new_message_record(message_id, now_ms(), channel, payload);
        self.store.append_message(&message)?;
        self.messages.push(message);
        self.next_message_id += 1;
        if mirror_to_inbox {
            self.refresh_inbox_mirror()?;
        }
        Ok(message_id)
    }

    fn fetch_messages(
        &self,
        channel: String,
        cursor_key: String,
        limit: Option<u32>,
        after_message_id: Option<u64>,
        targeted_only: bool,
        include_all_targets: bool,
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
        let fetch_after_message_id = last_message_id.max(after_message_id.unwrap_or(0));
        let latest_message_id = self
            .messages
            .iter()
            .filter(|message| message.channel == channel)
            .map(|message| message.message_id)
            .max()
            .unwrap_or(0);
        let target_route = RouteKey::from_cursor_key(&cursor_key);
        let mut messages = Vec::new();
        for message in self.messages.iter().filter(|message| {
            message.channel == channel
                && message.message_id > fetch_after_message_id
                && (include_all_targets
                    || message_matches_cursor_route(message, target_route.as_ref()))
                && (!targeted_only || message_targets_cursor_route(message, target_route.as_ref()))
        }) {
            if messages.len() >= limit as usize {
                break;
            }
            let candidate = message_to_ipc(message);
            if fetch_response_fits(
                &channel,
                &cursor_key,
                last_message_id,
                latest_message_id,
                &messages,
                &candidate,
            )? {
                messages.push(candidate);
                continue;
            }
            if messages.is_empty() {
                let truncated = truncated_ipc_message(candidate);
                if fetch_response_fits(
                    &channel,
                    &cursor_key,
                    last_message_id,
                    latest_message_id,
                    &messages,
                    &truncated,
                )? {
                    messages.push(truncated);
                }
            }
            break;
        }

        Ok(Response::Messages {
            protocol: PROTOCOL_VERSION,
            channel,
            cursor_key,
            last_message_id,
            latest_message_id,
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
        let latest_channel_message_id = self
            .messages
            .iter()
            .filter(|message| message.channel == channel)
            .map(|message| message.message_id)
            .max()
            .unwrap_or(0);
        if through_message_id > latest_channel_message_id {
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

    fn commit_cursor_if_current(
        &mut self,
        channel: String,
        cursor_key: String,
        expected_last_message_id: u64,
        through_message_id: u64,
    ) -> Result<Response> {
        validate_name("channel", &channel)?;
        validate_name("cursor_key", &cursor_key)?;
        let current = self
            .cursors
            .get(&(channel.clone(), cursor_key.clone()))
            .copied()
            .unwrap_or(0);
        if current != expected_last_message_id {
            return Ok(Response::CursorCommitStale {
                protocol: PROTOCOL_VERSION,
                channel,
                cursor_key,
                last_message_id: current,
            });
        }
        self.commit_cursor(channel, cursor_key, through_message_id)
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

    fn record_watcher_run(
        &mut self,
        run: watcher::WatcherRunResult,
        target_harness: Option<String>,
        target_session_id: Option<String>,
        source_event_id: Option<u64>,
    ) -> Result<Response> {
        let summary = summarize_watcher_run(&run);
        let mut message_ids = Vec::new();
        for message in &run.messages {
            message_ids.push(self.enqueue_watcher_message(
                message,
                target_harness.as_deref(),
                target_session_id.as_deref(),
                source_event_id,
            )?);
        }
        if !run.messages.is_empty() && !run.statuses.iter().any(status_should_queue_diagnostic) {
            self.clear_reported_failures_for(
                &run.watcher,
                target_harness.as_deref(),
                target_session_id.as_deref(),
            );
        }
        for status in &run.statuses {
            self.record_watcher_status(
                status,
                target_harness.as_deref(),
                target_session_id.as_deref(),
                source_event_id,
            )?;
            if let Some(message_id) = self.maybe_enqueue_status_diagnostic(
                status,
                target_harness.as_deref(),
                target_session_id.as_deref(),
                source_event_id,
            )? {
                message_ids.push(message_id);
            }
        }
        self.record_watcher_activity(
            &run.watcher,
            &run.tick_id,
            &summary,
            target_harness.as_deref(),
            target_session_id.as_deref(),
            source_event_id,
        )?;
        Ok(Response::WatcherRun {
            protocol: PROTOCOL_VERSION,
            watcher: run.watcher,
            tick_id: run.tick_id,
            message_ids,
            messages: run.messages,
            statuses: run.statuses,
            summary,
        })
    }

    fn enqueue_watcher_message(
        &mut self,
        message: &WatcherMessage,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
        source_event_id: Option<u64>,
    ) -> Result<u64> {
        let mut payload = serde_json::json!({
            "watcher": message.watcher,
            "tick_id": message.tick_id,
            "severity": message.severity,
            "refs": message.refs,
            "text": message.text,
            "usage": message.usage,
        });
        if let Some(harness) = target_harness {
            payload["target_harness"] = serde_json::Value::String(harness.to_owned());
        }
        if let Some(session_id) = target_session_id {
            payload["target_session_id"] = serde_json::Value::String(session_id.to_owned());
        }
        if let Some(event_id) = source_event_id {
            payload["source_event_id"] = serde_json::Value::Number(event_id.into());
        }
        self.enqueue_message("hook".to_owned(), payload)
    }

    fn record_watcher_status(
        &mut self,
        status: &WatcherStatusEvent,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
        source_event_id: Option<u64>,
    ) -> Result<()> {
        let record = new_watcher_status_record(
            status.watcher.clone(),
            status.outcome.clone(),
            now_ms(),
            Some(status.tick_id.clone()),
            Some(status.text.clone()),
            Some(details_with_route(
                status.details.clone(),
                target_harness,
                target_session_id,
                source_event_id,
            )),
        );
        self.store.append_watcher_status(&record)?;
        self.watcher_statuses
            .insert(watcher_status_key(&record), record);
        Ok(())
    }

    fn record_watcher_activity(
        &mut self,
        watcher: &str,
        tick_id: &str,
        summary: &WatcherRunSummary,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
        source_event_id: Option<u64>,
    ) -> Result<()> {
        let record = new_watcher_status_record(
            watcher.to_owned(),
            summary.state.clone(),
            now_ms(),
            Some(tick_id.to_owned()),
            Some(summary.text.clone()),
            Some(details_with_route(
                serde_json::json!({
                "severity": summary.severity,
                "message_count": summary.message_count,
                "status_count": summary.status_count,
                }),
                target_harness,
                target_session_id,
                source_event_id,
            )),
        );
        self.store.append_watcher_status(&record)?;
        self.watcher_statuses
            .insert(watcher_status_key(&record), record);
        Ok(())
    }

    fn watcher_statuses_response(&self) -> Response {
        let watchers = self
            .watcher_statuses
            .values()
            .map(watcher_status_to_ipc)
            .collect();
        Response::WatcherStatuses {
            protocol: PROTOCOL_VERSION,
            watchers,
        }
    }

    fn record_watch_heartbeat(
        &mut self,
        active: bool,
        profiles: Vec<String>,
        pid: u32,
        tick: Option<u64>,
        stale_after_ms: u64,
    ) -> IpcWatchStatus {
        self.watch = IpcWatchStatus {
            active,
            profiles,
            pid: Some(pid),
            tick,
            updated_at_ms: Some(now_ms()),
            stale_after_ms: Some(stale_after_ms),
        };
        self.watch_status()
    }

    fn watch_status(&self) -> IpcWatchStatus {
        let mut watch = self.watch.clone();
        if watch.active
            && watch.updated_at_ms.zip(watch.stale_after_ms).is_some_and(
                |(updated_at_ms, stale_after_ms)| {
                    now_ms().saturating_sub(updated_at_ms) > stale_after_ms
                },
            )
        {
            watch.active = false;
        }
        watch
    }

    fn stop_watch(&mut self) -> Result<(bool, IpcWatchStatus)> {
        let mut watch = self.watch_status();
        if !watch.active {
            self.watch = watch.clone();
            return Ok((false, watch));
        }
        if let Some(pid) = watch.pid {
            let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            if result == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        watch.active = false;
        watch.updated_at_ms = Some(now_ms());
        self.watch = watch.clone();
        Ok((true, watch))
    }

    fn ensure_watcher_check_in(
        &mut self,
        watcher: String,
        target_harness: Option<String>,
        target_session_id: Option<String>,
    ) -> Result<Response> {
        validate_name("watcher", &watcher)?;
        if let Some(harness) = target_harness.as_deref() {
            validate_name("target_harness", harness)?;
        }
        if let Some(session_id) = target_session_id.as_deref() {
            validate_name("target_session_id", session_id)?;
        }
        if let Some(message_id) = self.watcher_check_in_id(
            &watcher,
            target_harness.as_deref(),
            target_session_id.as_deref(),
        ) {
            return Ok(Response::WatcherCheckIn {
                protocol: PROTOCOL_VERSION,
                watcher,
                message_id: Some(message_id),
            });
        }

        let mut payload = serde_json::json!({
            "kind": "watcher_check_in",
            "watcher": watcher.clone(),
            "severity": "info",
            "refs": [],
            "text": watcher_check_in_text(&watcher),
            "usage": null,
        });
        if let Some(harness) = target_harness {
            payload["target_harness"] = serde_json::Value::String(harness);
        }
        if let Some(session_id) = target_session_id {
            payload["target_session_id"] = serde_json::Value::String(session_id);
        }
        let message_id = self.enqueue_message("hook".to_owned(), payload)?;
        Ok(Response::WatcherCheckIn {
            protocol: PROTOCOL_VERSION,
            watcher,
            message_id: Some(message_id),
        })
    }

    fn watcher_check_in_id(
        &self,
        watcher: &str,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
    ) -> Option<u64> {
        self.messages.iter().find_map(|message| {
            (message.channel == "hook"
                && payload_str(&message.payload, "kind") == Some("watcher_check_in")
                && payload_str(&message.payload, "watcher") == Some(watcher)
                && payload_str(&message.payload, "target_harness") == target_harness
                && payload_str(&message.payload, "target_session_id") == target_session_id)
                .then_some(message.message_id)
        })
    }

    fn maybe_enqueue_status_diagnostic(
        &mut self,
        status: &WatcherStatusEvent,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
        source_event_id: Option<u64>,
    ) -> Result<Option<u64>> {
        let Some(text) = status_diagnostic_text(status) else {
            return Ok(None);
        };
        let key = reported_failure_key(
            &status.watcher,
            &status.outcome,
            &status.text,
            target_harness,
            target_session_id,
        );
        if !self.reported_failures.insert(key) {
            return Ok(None);
        }
        let mut payload = serde_json::json!({
            "watcher": status.watcher,
            "tick_id": status.tick_id,
            "severity": status.severity,
            "refs": [],
            "text": text,
            "usage": null,
        });
        if let Some(harness) = target_harness {
            payload["target_harness"] = serde_json::Value::String(harness.to_owned());
        }
        if let Some(session_id) = target_session_id {
            payload["target_session_id"] = serde_json::Value::String(session_id.to_owned());
        }
        if let Some(event_id) = source_event_id {
            payload["source_event_id"] = serde_json::Value::Number(event_id.into());
        }
        self.enqueue_message("hook".to_owned(), payload).map(Some)
    }

    fn clear_reported_failures_for(
        &mut self,
        watcher: &str,
        target_harness: Option<&str>,
        target_session_id: Option<&str>,
    ) {
        let prefix = reported_failure_prefix(watcher, target_harness, target_session_id);
        self.reported_failures
            .retain(|key| !key.starts_with(&prefix));
    }

    fn refresh_inbox_mirror(&mut self) -> Result<()> {
        let text = render_inbox_mirror(&self.messages);
        if let Some(parent) = self.store.paths().inbox_path().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(self.store.paths().inbox_path(), text)?;
        set_private_file_permissions(self.store.paths().inbox_path())?;
        Ok(())
    }
}

fn status_should_queue_diagnostic(status: &WatcherStatusEvent) -> bool {
    status_diagnostic_text(status).is_some()
}

fn details_with_route(
    mut details: serde_json::Value,
    target_harness: Option<&str>,
    target_session_id: Option<&str>,
    source_event_id: Option<u64>,
) -> serde_json::Value {
    if !details.is_object() {
        details = serde_json::json!({ "details": details });
    }
    if let Some(harness) = target_harness {
        details["target_harness"] = serde_json::Value::String(harness.to_owned());
    }
    if let Some(session_id) = target_session_id {
        details["target_session_id"] = serde_json::Value::String(session_id.to_owned());
    }
    if let Some(event_id) = source_event_id {
        details["source_event_id"] = serde_json::Value::Number(event_id.into());
    }
    details
}

fn reported_failure_key(
    watcher: &str,
    outcome: &str,
    text: &str,
    target_harness: Option<&str>,
    target_session_id: Option<&str>,
) -> String {
    format!(
        "{}{}:{}",
        reported_failure_prefix(watcher, target_harness, target_session_id),
        outcome,
        text
    )
}

fn reported_failure_prefix(
    watcher: &str,
    target_harness: Option<&str>,
    target_session_id: Option<&str>,
) -> String {
    format!(
        "{}:{}:{}:",
        watcher,
        target_harness.unwrap_or(""),
        target_session_id.unwrap_or("")
    )
}

fn status_diagnostic_text(status: &WatcherStatusEvent) -> Option<String> {
    match status.outcome.as_str() {
        "api_failure" => Some(format!(
            "Watcher `{}` reported API failure: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "codex_cli_failed" => Some(format!(
            "Watcher `{}` could not run Codex CLI: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "repo_context_failed" => Some(format!(
            "Watcher `{}` could not build repo context: {}. Watcher ran without the repo map; further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "nonzero_exit" => Some(format!(
            "Watcher `{}` exited unsuccessfully: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "output_limit_exceeded" => Some(format!(
            "Watcher `{}` exceeded its output limit: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "malformed_stdout" => Some(format!(
            "Watcher `{}` emitted malformed output: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        "unsupported_stdout_event" => Some(format!(
            "Watcher `{}` emitted unsupported output: {}. Further identical failures are suppressed until recovery.",
            status.watcher, status.text
        )),
        _ => None,
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

fn fetch_response_fits(
    channel: &str,
    cursor_key: &str,
    last_message_id: u64,
    latest_message_id: u64,
    messages: &[IpcMessage],
    candidate: &IpcMessage,
) -> Result<bool> {
    let mut candidate_messages = messages.to_vec();
    candidate_messages.push(candidate.clone());
    let response = Response::Messages {
        protocol: PROTOCOL_VERSION,
        channel: channel.to_owned(),
        cursor_key: cursor_key.to_owned(),
        last_message_id,
        latest_message_id,
        messages: candidate_messages,
    };
    Ok(serde_json::to_vec(&response)?.len() <= ipc::MAX_FRAME_SIZE)
}

fn truncated_ipc_message(message: IpcMessage) -> IpcMessage {
    let mut payload = serde_json::Map::new();
    for key in [
        "watcher",
        "severity",
        "kind",
        "target_harness",
        "target_session_id",
        "source_event_id",
    ] {
        if let Some(value) = message.payload.get(key) {
            payload.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(refs) = message.payload.get("refs") {
        payload.insert("refs".to_owned(), refs.clone());
    }
    let text = payload_text(&message.payload);
    payload.insert(
        "text".to_owned(),
        serde_json::Value::String(format!(
            "{}\n[extra-eyes truncated oversized queued message for hook transport]",
            prefix_by_char_boundary(&text, 128 * 1024)
        )),
    );
    payload.insert("truncated".to_owned(), serde_json::Value::Bool(true));
    IpcMessage {
        payload: serde_json::Value::Object(payload),
        ..message
    }
}

fn watcher_status_to_ipc(status: &WatcherStatusRecord) -> IpcWatcherStatus {
    let severity = status
        .details
        .as_ref()
        .and_then(|details| details.get("severity"))
        .and_then(|value| value.as_str())
        .unwrap_or("info")
        .to_owned();
    IpcWatcherStatus {
        watcher: status.watcher.clone(),
        status: status.status.clone(),
        severity,
        text: status
            .message
            .clone()
            .unwrap_or_else(|| status.status.clone()),
        tick_id: status.tick_id.clone(),
        updated_at_ms: status.updated_at_ms,
        target_harness: status
            .details
            .as_ref()
            .and_then(|details| details.get("target_harness"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        target_session_id: status
            .details
            .as_ref()
            .and_then(|details| details.get("target_session_id"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        source_event_id: status
            .details
            .as_ref()
            .and_then(|details| details.get("source_event_id"))
            .and_then(|value| value.as_u64()),
    }
}

fn summarize_watcher_run(run: &watcher::WatcherRunResult) -> WatcherRunSummary {
    let mut severity = "info";
    for message in &run.messages {
        severity = max_severity(severity, &message.severity);
    }
    for status in &run.statuses {
        severity = max_severity(severity, &status.severity);
    }

    let (state, text) = if let Some(message) = run.messages.first() {
        (
            if severity == "info" {
                "message"
            } else {
                severity
            },
            summarize_text(&message.text),
        )
    } else if let Some(status) = run.statuses.first() {
        (status.outcome.as_str(), summarize_text(&status.text))
    } else {
        ("quiet", "no issues".to_owned())
    };

    WatcherRunSummary {
        state: state.to_owned(),
        severity: severity.to_owned(),
        text,
        message_count: run.messages.len(),
        status_count: run.statuses.len(),
    }
}

fn max_severity<'a>(left: &'a str, right: &'a str) -> &'a str {
    if severity_rank(right) > severity_rank(left) {
        right
    } else {
        left
    }
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 3,
        "warning" | "warn" => 2,
        "info" => 1,
        _ => 0,
    }
}

fn summarize_text(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= 180 {
        return normalized;
    }
    format!("{}...", prefix_by_char_boundary(&normalized, 177))
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

fn validate_name(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(EyesError::Protocol(format!("{label} cannot be empty")));
    }
    Ok(())
}

fn message_matches_cursor_route(message: &MessageRecord, route: Option<&RouteKey>) -> bool {
    let Some(target_session_id) = payload_str(&message.payload, "target_session_id") else {
        return true;
    };
    let Some(route) = route else {
        return false;
    };
    if route.session_id != target_session_id {
        return false;
    }
    payload_str(&message.payload, "target_harness")
        .map(|target_harness| target_harness == route.harness)
        .unwrap_or(false)
}

fn message_targets_cursor_route(message: &MessageRecord, route: Option<&RouteKey>) -> bool {
    let Some(target_session_id) = payload_str(&message.payload, "target_session_id") else {
        return false;
    };
    let Some(route) = route else {
        return false;
    };
    if route.session_id != target_session_id {
        return false;
    }
    payload_str(&message.payload, "target_harness")
        .map(|target_harness| target_harness == route.harness)
        .unwrap_or(false)
}

fn render_inbox_mirror(messages: &[MessageRecord]) -> String {
    let mut text = String::from(
        "# Extra Eyes Inbox\n\n\
        Generated by `eyes`. Watcher messages are also delivered through harness hooks when configured.\n",
    );
    let hook_messages = messages
        .iter()
        .filter(|message| message.channel == "hook")
        .collect::<Vec<_>>();

    if hook_messages.is_empty() {
        text.push_str("\nNo watcher messages yet.\n");
        return text;
    }

    for message in hook_messages.iter().rev().take(50).rev() {
        let watcher = payload_str(&message.payload, "watcher").unwrap_or("unknown");
        let severity = payload_str(&message.payload, "severity").unwrap_or("info");
        text.push_str(&format!(
            "\n## Message {}\n\n- watcher: `{}`\n- severity: `{}`\n",
            message.message_id, watcher, severity
        ));
        if let Some(tick_id) = payload_str(&message.payload, "tick_id") {
            text.push_str(&format!("- tick: `{tick_id}`\n"));
        }
        if let Some(harness) = payload_str(&message.payload, "target_harness") {
            if let Some(session_id) = payload_str(&message.payload, "target_session_id") {
                text.push_str(&format!("- target: `{harness}:{session_id}`\n"));
            }
        }
        if let Some(source_event_id) = message
            .payload
            .get("source_event_id")
            .and_then(|id| id.as_u64())
        {
            text.push_str(&format!("- source event: `{source_event_id}`\n"));
        }
        text.push('\n');
        text.push_str(&payload_text(&message.payload));
        text.push('\n');
    }

    text
}

fn payload_str<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn watcher_check_in_text(watcher: &str) -> String {
    format!(
        "Check-in: watcher `{watcher}` is connected. In your next user-facing reply, briefly say that Extra Eyes watcher `{watcher}` is connected and you are receiving its messages. After that, mention watcher input only when it changes what you do."
    )
}

fn payload_text(payload: &serde_json::Value) -> String {
    if let Some(text) = payload_str(payload, "text") {
        return text.to_owned();
    }
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
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
