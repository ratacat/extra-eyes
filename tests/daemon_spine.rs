use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use extra_eyes::context::ContextBudgetReport;
use extra_eyes::conversation::ConversationEvent;
use extra_eyes::delivery::DEFAULT_HOOK_OUTPUT_BUDGET_BYTES;
use extra_eyes::identity::ProjectIdentity;
use extra_eyes::ipc::{send_request, Request, Response, PROTOCOL_VERSION};
use extra_eyes::paths::ProjectPaths;
use extra_eyes::watcher::WatcherContext;

const FIXTURE_WATCHER_TIMEOUT_MS: u64 = 5_000;

fn bin(name: &str) -> &'static str {
    match name {
        "eyesd" => env!("CARGO_BIN_EXE_eyesd"),
        "eyes" => env!("CARGO_BIN_EXE_eyes"),
        _ => unreachable!(),
    }
}

#[test]
fn daemon_reports_status_and_cleans_up_after_stop() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);

    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = status
        .get("socket_path")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    let pid_path = std::path::Path::new(&socket_path)
        .parent()
        .unwrap()
        .join("eyesd.pid.json");

    assert_eq!(
        status.get("project_root").and_then(Value::as_str).unwrap(),
        project.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(project.join(".eyes/state").is_dir());
    assert!(pid_path.exists());

    let stop = Command::new(bin("eyesd"))
        .args(["stop", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let exit = daemon.wait().unwrap();
    assert!(exit.success());
    assert!(!std::path::Path::new(&socket_path).exists());
    assert!(!pid_path.exists());
}

#[test]
fn daemon_start_defaults_to_detached_background_process() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let start = Command::new(bin("eyesd"))
        .args(["start", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let started: Value = serde_json::from_slice(&start.stdout).unwrap();
    let log_path = std::path::PathBuf::from(started["log_path"].as_str().unwrap());
    assert_eq!(
        started["project_root"].as_str().unwrap(),
        project.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(started["pid"].as_u64().unwrap() > 0);
    assert!(log_path.exists());

    let status = Command::new(bin("eyesd"))
        .args(["status", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["pid"], started["pid"]);

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn daemon_refuses_a_second_start_for_the_same_project() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let second = Command::new(bin("eyesd"))
        .args(["start", "--foreground", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already"));

    let stop = Command::new(bin("eyesd"))
        .args(["stop", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn daemon_cleans_up_after_sigterm() {
    assert_signal_cleanup(libc::SIGTERM);
}

#[test]
fn daemon_cleans_up_after_sigint() {
    assert_signal_cleanup(libc::SIGINT);
}

#[test]
fn daemon_recovers_from_orphaned_socket_path() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let identity = ProjectIdentity::from_root(project.clone()).unwrap();
    let paths = ProjectPaths::from_identity_with_runtime_base(identity, runtime.clone()).unwrap();
    paths.ensure().unwrap();
    let orphan = UnixListener::bind(paths.socket_path()).unwrap();
    drop(orphan);
    assert!(paths.socket_path().exists());

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    assert_eq!(
        status.get("status").and_then(Value::as_str),
        Some("running")
    );

    let stop = Command::new(bin("eyesd"))
        .args(["stop", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn message_queue_and_cursor_survive_restart() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first = enqueue(&socket_path, "hook", serde_json::json!({"body":"one"}));
    let second = enqueue(&socket_path, "hook", serde_json::json!({"body":"two"}));
    assert!(second > first);
    commit(&socket_path, "hook", "codex:s1:hook", first);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());

    let mut restarted = spawn_daemon(&project, &runtime);
    let restarted_status = wait_for_status(&project, &runtime, &mut restarted);
    let restarted_socket = socket_path_from_status(&restarted_status);
    let messages = fetch(&restarted_socket, "hook", "codex:s1:hook", None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, second);
    assert_eq!(messages[0].payload, serde_json::json!({"body":"two"}));

    let third = enqueue(
        &restarted_socket,
        "hook",
        serde_json::json!({"body":"three"}),
    );
    assert_eq!(third, second + 1);

    stop_daemon(&project, &runtime);
    assert!(restarted.wait().unwrap().success());
}

#[test]
fn fetch_does_not_advance_cursor_until_commit() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let message_id = enqueue(&socket_path, "hook", serde_json::json!({"body":"retry"}));

    assert_eq!(fetch(&socket_path, "hook", "claude:s1:hook", None).len(), 1);
    assert_eq!(fetch(&socket_path, "hook", "claude:s1:hook", None).len(), 1);
    commit(&socket_path, "hook", "claude:s1:hook", message_id);
    assert!(fetch(&socket_path, "hook", "claude:s1:hook", None).is_empty());
    let committed = commit(&socket_path, "hook", "claude:s1:hook", message_id - 1);
    assert_eq!(committed, message_id);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn channels_and_cursors_are_isolated() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let hook_message = enqueue(&socket_path, "hook", serde_json::json!({"body":"hook"}));
    let inbox_message = enqueue(&socket_path, "inbox", serde_json::json!({"body":"inbox"}));

    let hook_a = fetch(&socket_path, "hook", "session:a", None);
    assert_eq!(hook_a.len(), 1);
    assert_eq!(hook_a[0].message_id, hook_message);
    commit(&socket_path, "hook", "session:a", hook_message);

    let hook_b = fetch(&socket_path, "hook", "session:b", None);
    assert_eq!(hook_b.len(), 1);
    assert_eq!(hook_b[0].message_id, hook_message);

    let inbox_a = fetch(&socket_path, "inbox", "session:a", None);
    assert_eq!(inbox_a.len(), 1);
    assert_eq!(inbox_a[0].message_id, inbox_message);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn inbox_mirror_receives_watcher_messages_for_file_fallback() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "fallback-visible",
            "--watcher",
            "fallback",
            "--severity",
            "warning",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );

    let inbox = fs::read_to_string(project.join(".eyes/inbox.md")).unwrap();
    assert!(inbox.contains("# Extra Eyes Inbox"));
    assert!(inbox.contains("- watcher: `fallback`"));
    assert!(inbox.contains("- severity: `warning`"));
    assert!(inbox.contains("fallback-visible"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn rejects_invalid_cursor_commits_and_fetch_limits() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let message_id = enqueue(&socket_path, "hook", serde_json::json!({"body":"one"}));

    let future_ack = send_request(
        &socket_path,
        &Request::CommitCursor {
            protocol: PROTOCOL_VERSION,
            channel: "hook".to_owned(),
            cursor_key: "session:a".to_owned(),
            through_message_id: message_id + 100,
        },
    )
    .unwrap();
    assert!(matches!(future_ack, Response::Error { .. }));

    let zero_limit = send_request(
        &socket_path,
        &Request::FetchMessages {
            protocol: PROTOCOL_VERSION,
            channel: "hook".to_owned(),
            cursor_key: "session:a".to_owned(),
            limit: Some(0),
        },
    )
    .unwrap();
    assert!(matches!(zero_limit, Response::Error { .. }));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn socket_round_trips_empty_small_and_large_payloads() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let large = "x".repeat(128 * 1024);

    enqueue(&socket_path, "hook", serde_json::json!({}));
    enqueue(&socket_path, "hook", serde_json::json!({"body":"small"}));
    enqueue(&socket_path, "hook", serde_json::json!({"body":large}));

    let messages = fetch(&socket_path, "hook", "session:large", Some(10));
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].payload, serde_json::json!({}));
    assert_eq!(messages[1].payload, serde_json::json!({"body":"small"}));
    assert_eq!(
        messages[2].payload["body"].as_str().unwrap().len(),
        128 * 1024
    );
    assert!(matches!(
        send_request(
            &socket_path,
            &Request::Ping {
                protocol: PROTOCOL_VERSION
            }
        )
        .unwrap(),
        Response::Pong { .. }
    ));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn concurrent_ipc_writers_do_not_corrupt_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let handles = (0..20)
        .map(|index| {
            let socket_path = socket_path.clone();
            thread::spawn(move || {
                enqueue(
                    &socket_path,
                    "hook",
                    serde_json::json!({"watcher":"concurrent","idx":index}),
                )
            })
        })
        .collect::<Vec<_>>();

    let ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let unique_ids = ids.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(unique_ids.len(), 20);

    let messages = fetch(&socket_path, "hook", "session:concurrent", Some(25));
    assert_eq!(messages.len(), 20);
    let indexes = messages
        .iter()
        .map(|message| message.payload["idx"].as_u64().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(indexes, (0..20).collect::<BTreeSet<_>>());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn runtime_dir_and_socket_permissions_are_owner_only() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let runtime_dir = socket_path.parent().unwrap();

    let runtime_mode = fs::metadata(runtime_dir).unwrap().permissions().mode() & 0o777;
    let socket_mode = fs::metadata(&socket_path).unwrap().permissions().mode() & 0o777;

    assert_eq!(runtime_mode, 0o700);
    assert_eq!(socket_mode & 0o077, 0);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn raw_watcher_spawn_receives_context_and_enqueues_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();
    let capture = temp.path().join("envelope.json");
    let meta = temp.path().join("meta.txt");
    let script = temp.path().join("echo-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
meta="$2"
cat > "$capture"
{
  pwd
  printf '%s\n' "$EXTRA_FIXTURE"
  printf '%s\n' "$EXTRA_EYES_WATCHER_NAME"
  printf '%s\n' "$EXTRA_EYES_TICK_ID"
} > "$meta"
printf '%s\n' '{"v":1,"type":"message","severity":"warning","text":"spawn-ok","refs":[{"path":"src/lib.rs","line":1}],"usage":{"units":2}}'
"#,
    );
    write_raw_profile(
        &watchers,
        "echo",
        &[&script, &capture, &meta],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        Some(("EXTRA_FIXTURE", "fixture-env")),
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let response = run_watcher(
        &socket_path,
        "echo",
        "tick-echo",
        WatcherContext {
            files: vec!["src/lib.rs".to_owned()],
            diff: "diff --git a/src/lib.rs b/src/lib.rs".to_owned(),
            conversation: vec![ConversationEvent {
                harness: "claude-code".to_owned(),
                event: "user_prompt_submit".to_owned(),
                role: "user".to_owned(),
                text: "hello".to_owned(),
                source: "interactive".to_owned(),
                session_id: "session-1".to_owned(),
                timestamp_ms: 123,
            }],
            budget: ContextBudgetReport::default(),
        },
    );

    let message_ids = match response {
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } => {
            assert!(statuses.is_empty());
            message_ids
        }
        other => panic!("unexpected watcher response: {other:?}"),
    };
    assert_eq!(message_ids.len(), 1);

    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(envelope["v"], 1);
    assert_eq!(envelope["watcher"], "echo");
    assert_eq!(envelope["tick_id"], "tick-echo");
    assert_eq!(envelope["prompt"], "Prompt for echo");
    assert_eq!(envelope["context"]["files"][0], "src/lib.rs");
    assert_eq!(
        envelope["context"]["diff"],
        "diff --git a/src/lib.rs b/src/lib.rs"
    );
    assert_eq!(envelope["context"]["conversation"][0]["role"], "user");
    assert_eq!(envelope["context"]["conversation"][0]["text"], "hello");

    let meta = fs::read_to_string(meta).unwrap();
    let meta_lines = meta.lines().collect::<Vec<_>>();
    assert_eq!(
        meta_lines[0],
        project.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(meta_lines[1], "fixture-env");
    assert_eq!(meta_lines[2], "echo");
    assert_eq!(meta_lines[3], "tick-echo");

    let messages = fetch(&socket_path, "hook", "session:watcher", None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload["watcher"], "echo");
    assert_eq!(messages[0].payload["severity"], "warning");
    assert_eq!(messages[0].payload["refs"][0]["path"], "src/lib.rs");
    assert_eq!(messages[0].payload["text"], "spawn-ok");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_malformed_stdout_and_nonzero_exit_are_isolated() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();
    let bad = temp.path().join("bad-watcher.sh");
    write_executable(
        &bad,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"before"}'
printf '%s\n' 'not json'
printf '%s\n' '{"v":1,"type":"banana","text":"unsupported"}'
printf '%s\n' '{"v":1,"type":"message","severity":"error","text":"after"}'
exit 2
"#,
    );
    let good = temp.path().join("good-watcher.sh");
    write_executable(
        &good,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"sibling-ok"}'
"#,
    );
    write_raw_profile(
        &watchers,
        "bad",
        &[&bad],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    write_raw_profile(
        &watchers,
        "good",
        &[&good],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let response = run_watcher(&socket_path, "bad", "tick-bad", WatcherContext::default());
    let statuses = match response {
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } => {
            assert_eq!(message_ids.len(), 2);
            statuses
        }
        other => panic!("unexpected watcher response: {other:?}"),
    };
    let outcomes = statuses
        .iter()
        .map(|status| status.outcome.as_str())
        .collect::<Vec<_>>();
    assert!(outcomes.contains(&"malformed_stdout"));
    assert!(outcomes.contains(&"unsupported_stdout_event"));
    assert!(outcomes.contains(&"nonzero_exit"));
    assert!(daemon_ping(&socket_path));

    let sibling = run_watcher(&socket_path, "good", "tick-good", WatcherContext::default());
    assert!(matches!(
        sibling,
        Response::WatcherRun {
            message_ids,
            ..
        } if message_ids.len() == 1
    ));
    let status_rows = fs::read_to_string(project.join(".eyes/state/watcher-status.jsonl")).unwrap();
    assert!(status_rows.contains("malformed_stdout"));
    assert!(status_rows.contains("nonzero_exit"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_timeout_and_cost_caps_are_enforced() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let sleeper = temp.path().join("sleep-watcher.sh");
    write_executable(
        &sleeper,
        r#"#!/bin/sh
cat >/dev/null
sleep 5
"#,
    );
    let cost = temp.path().join("cost-watcher.sh");
    write_executable(
        &cost,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"a","usage":{"units":7}}'
printf '%s\n' '{"v":1,"type":"message","text":"b","usage":{"units":4}}'
"#,
    );
    let marker = temp.path().join("should-not-exist");
    let skipped = temp.path().join("skipped-watcher.sh");
    write_executable(
        &skipped,
        &format!(
            "#!/bin/sh\nprintf spawned > {}\n",
            shell_quote(marker.to_str().unwrap())
        ),
    );
    write_raw_profile(&watchers, "sleepy", &[&sleeper], Some(50), Some(10), None);
    write_raw_profile(
        &watchers,
        "costly",
        &[&cost],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    write_raw_profile(
        &watchers,
        "skipped",
        &[&skipped],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(0),
        None,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let started = Instant::now();
    let timeout_response = run_watcher(
        &socket_path,
        "sleepy",
        "tick-timeout",
        WatcherContext::default(),
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        timeout_response,
        Response::WatcherRun {
            statuses,
            ..
        } if statuses.iter().any(|status| status.outcome == "timeout")
    ));
    assert!(daemon_ping(&socket_path));

    let cost_response = run_watcher(
        &socket_path,
        "costly",
        "tick-cost",
        WatcherContext::default(),
    );
    match cost_response {
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } => {
            assert_eq!(message_ids.len(), 2);
            assert!(statuses
                .iter()
                .any(|status| status.outcome == "cost_limit_exceeded"));
        }
        other => panic!("unexpected watcher response: {other:?}"),
    }

    let skipped_response = run_watcher(
        &socket_path,
        "skipped",
        "tick-skip",
        WatcherContext::default(),
    );
    assert!(matches!(
        skipped_response,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.is_empty()
            && statuses.iter().any(|status| status.outcome == "cost_limit_exceeded")
    ));
    assert!(!marker.exists());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_api_failure_is_reported_once_until_recovery() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let counter = temp.path().join("api-counter");
    let script = temp.path().join("api-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
counter="$1"
count=0
if [ -f "$counter" ]; then
  count=$(cat "$counter")
fi
count=$((count + 1))
printf '%s' "$count" > "$counter"
cat >/dev/null
case "$count" in
  1|2|4)
    printf '%s\n' '{"v":1,"type":"status","severity":"error","outcome":"api_failure","text":"rate_limit"}'
    ;;
  *)
    printf '%s\n' '{"v":1,"type":"message","text":"recovered"}'
    ;;
esac
"#,
    );
    write_raw_profile(
        &watchers,
        "api",
        &[&script, &counter],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first = run_watcher(&socket_path, "api", "tick-api-1", WatcherContext::default());
    assert!(matches!(
        first,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.len() == 1
            && statuses.iter().any(|status| status.outcome == "api_failure")
    ));
    let first_fetch = fetch(&socket_path, "hook", "api-session", None);
    assert_eq!(first_fetch.len(), 1);
    assert!(first_fetch[0].payload["text"]
        .as_str()
        .unwrap()
        .contains("rate_limit"));
    commit(
        &socket_path,
        "hook",
        "api-session",
        first_fetch[0].message_id,
    );

    let second = run_watcher(&socket_path, "api", "tick-api-2", WatcherContext::default());
    assert!(matches!(
        second,
        Response::WatcherRun { message_ids, .. } if message_ids.is_empty()
    ));
    assert!(fetch(&socket_path, "hook", "api-session", None).is_empty());

    let recovered = run_watcher(&socket_path, "api", "tick-api-3", WatcherContext::default());
    assert!(matches!(
        recovered,
        Response::WatcherRun { message_ids, .. } if message_ids.len() == 1
    ));
    let recovered_fetch = fetch(&socket_path, "hook", "api-session", None);
    assert_eq!(recovered_fetch.len(), 1);
    commit(
        &socket_path,
        "hook",
        "api-session",
        recovered_fetch[0].message_id,
    );

    let fourth = run_watcher(&socket_path, "api", "tick-api-4", WatcherContext::default());
    assert!(matches!(
        fourth,
        Response::WatcherRun { message_ids, .. } if message_ids.len() == 1
    ));
    let fourth_fetch = fetch(&socket_path, "hook", "api-session", None);
    assert_eq!(fourth_fetch.len(), 1);
    assert!(fourth_fetch[0].payload["text"]
        .as_str()
        .unwrap()
        .contains("rate_limit"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_tick_runs_default_profile_with_git_diff_context() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    git(&project, &["init", "-q"]);
    fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> i32 { 42 }\n",
    )
    .unwrap();
    git(&project, &["add", "-N", "src/lib.rs"]);

    let capture = temp.path().join("tick-envelope.json");
    let script = temp.path().join("tick-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat > "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"tick-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "default",
        &[&script, &capture],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-manual", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(envelope["watcher"], "default");
    assert_eq!(envelope["tick_id"], "tick-manual");
    assert_eq!(envelope["context"]["files"][0], "src/lib.rs");
    assert!(envelope["context"]["diff"]
        .as_str()
        .unwrap()
        .contains("pub fn answer"));

    let fetched = Command::new(bin("eyes"))
        .args(["hook", "fetch", "--cursor-key", "tick-session", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(String::from_utf8(fetched.stdout)
        .unwrap()
        .contains("\"text\": \"tick-ok\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn feed_normalizes_harness_conversation_into_watcher_context() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let capture = temp.path().join("conversation-envelope.json");
    let script = temp.path().join("conversation-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat > "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"conversation-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "conversation",
        &[&script, &capture],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    for (harness, event, payload) in [
        (
            "claude-code",
            "UserPromptSubmit",
            r#"{"session_id":"cc-1","prompt":"fix this","timestamp_ms":10}"#,
        ),
        (
            "codex",
            "Stop",
            r#"{"session_id":"codex-1","last_assistant_message":"done","timestamp_ms":20}"#,
        ),
        (
            "pi",
            "input",
            r#"{"session_id":"pi-1","event":{"text":"review it","source":"interactive"},"timestamp_ms":30}"#,
        ),
    ] {
        let feed = Command::new(bin("eyes"))
            .args([
                "feed",
                "--harness",
                harness,
                "--event",
                event,
                "--payload-json",
                payload,
                "--project",
            ])
            .arg(&project)
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            feed.status.success(),
            "{}",
            String::from_utf8_lossy(&feed.stderr)
        );
    }

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-conversation", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );

    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    let conversation = envelope["context"]["conversation"].as_array().unwrap();
    assert_eq!(conversation.len(), 3);
    assert_eq!(conversation[0]["harness"], "claude-code");
    assert_eq!(conversation[0]["role"], "user");
    assert_eq!(conversation[0]["text"], "fix this");
    assert_eq!(conversation[1]["harness"], "codex");
    assert_eq!(conversation[1]["role"], "assistant");
    assert_eq!(conversation[1]["text"], "done");
    assert_eq!(conversation[2]["harness"], "pi");
    assert_eq!(conversation[2]["source"], "interactive");
    assert!(conversation[2].get("raw").is_none());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_context_respects_profile_budget() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let capture = temp.path().join("budget-envelope.json");
    let script = temp.path().join("budget-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat > "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"budget-ok"}'
"#,
    );
    fs::write(
        watchers.join("budget.toml"),
        format!(
            r#"
name = "budget"
default = false
prompt = "Prompt for budget"
harness = "raw"
model = "fixture"
[settings]
command = [{script}, {capture}]
timeout_ms = {timeout_ms}
cost_limit_units = 10
context_budget_bytes = 1800
"#,
            script = toml_string(script.to_str().unwrap()),
            capture = toml_string(capture.to_str().unwrap()),
            timeout_ms = FIXTURE_WATCHER_TIMEOUT_MS,
        ),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let response = run_watcher(
        &socket_path,
        "budget",
        "tick-budget",
        WatcherContext {
            files: vec!["src/lib.rs".to_owned()],
            diff: "x".repeat(20_000),
            conversation: vec![
                ConversationEvent {
                    harness: "claude-code".to_owned(),
                    event: "user_prompt_submit".to_owned(),
                    role: "user".to_owned(),
                    text: "old".to_owned(),
                    source: "interactive".to_owned(),
                    session_id: "s".to_owned(),
                    timestamp_ms: 1,
                },
                ConversationEvent {
                    harness: "claude-code".to_owned(),
                    event: "user_prompt_submit".to_owned(),
                    role: "user".to_owned(),
                    text: "new".to_owned(),
                    source: "interactive".to_owned(),
                    session_id: "s".to_owned(),
                    timestamp_ms: 2,
                },
            ],
            budget: ContextBudgetReport::default(),
        },
    );
    assert!(matches!(response, Response::WatcherRun { .. }));

    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    let context = &envelope["context"];
    assert_eq!(context["budget"]["max_bytes"], 1800);
    assert!(context["budget"]["used_bytes"].as_u64().unwrap() <= 1800);
    assert_eq!(context["budget"]["truncated"], true);
    assert!(context["budget"]["omitted_diff_bytes"].as_u64().unwrap() > 0);
    assert!(context["diff"]
        .as_str()
        .unwrap()
        .contains("[extra-eyes:diff-truncated"));
    assert_eq!(
        context["conversation"].as_array().unwrap().last().unwrap()["text"],
        "new"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn profile_precedence_bundled_default_and_no_restart_authoring_work() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let project_watchers = project.join(".eyes/watchers");
    let user_home = temp.path().join("home");
    let user_watchers = user_home.join("watchers");
    fs::create_dir_all(&project_watchers).unwrap();
    fs::create_dir_all(&user_watchers).unwrap();
    let script = temp.path().join("late-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"late-ok"}'
"#,
    );

    write_raw_profile(
        &user_watchers,
        "same",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    write_raw_profile(
        &project_watchers,
        "same",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    let resolved = Command::new(bin("eyes"))
        .args(["profile", "resolve", "same", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_HOME", &user_home)
        .output()
        .unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let json: Value = serde_json::from_slice(&resolved.stdout).unwrap();
    assert_eq!(json["source"], "project");

    let empty_project = temp.path().join("empty-project");
    let empty_home = temp.path().join("empty-home");
    fs::create_dir_all(&empty_project).unwrap();
    fs::create_dir_all(&empty_home).unwrap();
    let bundled = Command::new(bin("eyes"))
        .args(["profile", "resolve", "--json", "--project"])
        .arg(&empty_project)
        .env("EXTRA_EYES_HOME", &empty_home)
        .output()
        .unwrap();
    assert!(bundled.status.success());
    let bundled_json: Value = serde_json::from_slice(&bundled.stdout).unwrap();
    assert_eq!(bundled_json["source"], "bundled");
    assert_eq!(bundled_json["profile"]["name"], "general");

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    write_raw_profile(
        &project_watchers,
        "late",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    let tick = Command::new(bin("eyes"))
        .args(["tick", "late", "--tick-id", "tick-late", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_detects_edit_runs_default_profile_and_hook_fetches_message() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    git(&project, &["init", "-q"]);

    let script = temp.path().join("watch-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"watch-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "watcher",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    fs::write(project.join("src/lib.rs"), "pub fn seed() {}\n").unwrap();
    git(&project, &["add", "-N", "src/lib.rs"]);

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--debounce-ms",
            "50",
            "--max-ticks",
            "1",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let edit_started = Instant::now();
    let keep_editing = Arc::new(AtomicBool::new(true));
    let editor_project = project.clone();
    let editor_running = Arc::clone(&keep_editing);
    let editor = thread::spawn(move || {
        for index in 0..40 {
            if !editor_running.load(Ordering::Relaxed) {
                return;
            }
            fs::write(
                editor_project.join("src/lib.rs"),
                format!("pub fn watched_{index}() -> bool {{ true }}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
        }
    });
    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "watch-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(
        edit_started.elapsed() < Duration::from_secs(5),
        "edit-to-hook visibility took {:?}",
        edit_started.elapsed()
    );
    assert!(String::from_utf8(fetched.stdout)
        .unwrap()
        .contains("\"text\": \"watch-ok\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watch_fans_out_to_multiple_profiles_and_respects_cadence() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    git(&project, &["init", "-q"]);

    let run_log = temp.path().join("runs.log");
    let script = temp.path().join("fanout-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
run_log="$1"
cat >/dev/null
printf '%s:%s\n' "$EXTRA_EYES_WATCHER_NAME" "$EXTRA_EYES_TICK_ID" >> "$run_log"
printf '{"v":1,"type":"message","text":"%s-ok"}\n' "$EXTRA_EYES_WATCHER_NAME"
"#,
    );
    for (name, cadence) in [("fast", 1), ("slow", 2)] {
        fs::write(
            watchers.join(format!("{name}.toml")),
            format!(
                r#"
name = {name}
default = false
prompt = {prompt}
harness = "raw"
model = "fixture"
[settings]
command = [{script}, {run_log}]
timeout_ms = {timeout_ms}
cost_limit_units = 10
cadence_ticks = {cadence}
"#,
                name = toml_string(name),
                prompt = toml_string(&format!("Prompt for {name}")),
                script = toml_string(script.to_str().unwrap()),
                run_log = toml_string(run_log.to_str().unwrap()),
                timeout_ms = FIXTURE_WATCHER_TIMEOUT_MS,
            ),
        )
        .unwrap();
    }

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "fast",
            "slow",
            "--poll-ms",
            "25",
            "--debounce-ms",
            "50",
            "--max-ticks",
            "2",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let edit_started = Instant::now();
    let keep_editing = Arc::new(AtomicBool::new(true));
    let editor_project = project.clone();
    let editor_running = Arc::clone(&keep_editing);
    let editor = thread::spawn(move || {
        for index in 0..40 {
            if !editor_running.load(Ordering::Relaxed) {
                return;
            }
            fs::write(
                editor_project.join("src/lib.rs"),
                format!("pub fn edit_{index}() {{}}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
        }
    });

    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let log = fs::read_to_string(&run_log).unwrap();
    let fast_runs = log.lines().filter(|line| line.starts_with("fast:")).count();
    let slow_runs = log.lines().filter(|line| line.starts_with("slow:")).count();
    assert_eq!(fast_runs, 2, "{log}");
    assert_eq!(slow_runs, 1, "{log}");

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "fanout-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(
        edit_started.elapsed() < Duration::from_secs(5),
        "edit-to-hook visibility took {:?}",
        edit_started.elapsed()
    );
    let rendered = String::from_utf8(fetched.stdout).unwrap();
    assert_eq!(rendered.matches("\"text\": \"fast-ok\"").count(), 2);
    assert_eq!(rendered.matches("\"text\": \"slow-ok\"").count(), 1);
    assert!(rendered.contains("watcher=\"fast\""));
    assert!(rendered.contains("watcher=\"slow\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_ignores_internal_and_generated_paths() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join(".eyes/state")).unwrap();
    fs::create_dir_all(project.join("target/debug")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    let script = temp.path().join("ignored-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"should-not-run"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "watcher",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--debounce-ms",
            "25",
            "--max-ticks",
            "1",
            "--idle-timeout-ms",
            "250",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(75));
    fs::write(project.join(".eyes/state/messages.jsonl"), "ignored").unwrap();
    fs::write(project.join("target/debug/output"), "ignored").unwrap();
    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "ignored-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(fetched.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_is_silent_when_daemon_is_down() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let output = Command::new(bin("eyes"))
        .args(["hook", "fetch", "--cursor-key", "down-session", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn hook_codex_is_silent_on_malformed_payload_and_daemon_down() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let output = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", "not json");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let payload = r#"{"session_id":"codex-down","prompt":"daemon is down"}"#;
    let output = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn hook_trust_codex_writes_state_for_codex_hook_config() {
    let temp = TempDir::new().unwrap();
    let hooks_dir = temp.path().join("project/.codex");
    let codex_home = temp.path().join("codex-home");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    let hooks_config = hooks_dir.join("config.toml");
    let state_config = codex_home.join("config.toml");
    fs::write(
        &hooks_config,
        r#"[hooks]

[[hooks.UserPromptSubmit]]
matcher = "ignored"

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "eyes hook fetch --cursor-key codex:s1:hook"

[[hooks.Stop]]
matcher = "ignored"

[[hooks.Stop.hooks]]
type = "command"
command = "eyes feed --harness codex --event Stop --payload-json '{}'"
"#,
    )
    .unwrap();
    fs::write(&state_config, "[features]\nhooks = true\n").unwrap();

    let trust = Command::new(bin("eyes"))
        .args(["hook", "trust-codex", "--hooks-config"])
        .arg(&hooks_config)
        .args(["--state-config"])
        .arg(&state_config)
        .args(["--write", "--json"])
        .output()
        .unwrap();
    assert!(
        trust.status.success(),
        "{}",
        String::from_utf8_lossy(&trust.stderr)
    );

    let output: Value = serde_json::from_slice(&trust.stdout).unwrap();
    assert_eq!(output["entries"].as_array().unwrap().len(), 2);
    let canonical_hooks_config = fs::canonicalize(&hooks_config).unwrap();
    let written = fs::read_to_string(&state_config).unwrap();
    assert!(written.contains("[features]"));
    assert!(written.contains(&format!(
        "{}:user_prompt_submit:0:0",
        canonical_hooks_config.display()
    )));
    assert!(written.contains(&format!("{}:stop:0:0", canonical_hooks_config.display())));
    assert!(written.contains("trusted_hash = \"sha256:"));
}

#[test]
fn install_codex_writes_hooks_and_trust_state_idempotently() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("codex-home/config.toml");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();
    fs::write(
        &config,
        r#"[features]
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

    let first = Command::new(bin("eyes"))
        .args(["install", "codex", "--config"])
        .arg(&config)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .args(["--json"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(output["installed_events"].as_array().unwrap().len(), 3);
    assert_eq!(output["trust_entries"].as_array().unwrap().len(), 3);
    assert!(output["warnings"].as_array().unwrap().is_empty());

    let second = Command::new(bin("eyes"))
        .args(["install", "codex", "--config"])
        .arg(&config)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .args(["--json"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let output: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(output["trust_entries"].as_array().unwrap().len(), 3);

    let written = fs::read_to_string(&config).unwrap();
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
            .matches("hook codex --integration extra-eyes --event Stop")
            .count(),
        1
    );
    assert!(written.contains("command = \"echo existing\""));
    assert!(!written.contains("echo existing:"));
    assert_eq!(written.matches("async = false").count(), 3);
    assert_eq!(written.matches("trusted_hash = \"sha256:").count(), 3);
}

#[test]
fn setup_codex_alias_installs_hooks() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("codex-home/config.toml");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();

    let setup = Command::new(bin("eyes"))
        .args(["setup", "codex", "--config"])
        .arg(&config)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .args(["--json"])
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let output: Value = serde_json::from_slice(&setup.stdout).unwrap();
    assert_eq!(output["installed_events"].as_array().unwrap().len(), 3);

    let written = fs::read_to_string(&config).unwrap();
    assert!(written.contains("hook codex --integration extra-eyes --event UserPromptSubmit"));
    assert_eq!(written.matches("trusted_hash = \"sha256:").count(), 3);
}

#[test]
fn install_claude_code_writes_hooks_idempotently() {
    let temp = TempDir::new().unwrap();
    let settings = temp.path().join("claude/settings.json");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();
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

    for _ in 0..2 {
        let install = Command::new(bin("eyes"))
            .args(["install", "claude-code", "--settings"])
            .arg(&settings)
            .args(["--eyes-bin"])
            .arg(&eyes_bin)
            .args(["--json"])
            .output()
            .unwrap();
        assert!(
            install.status.success(),
            "{}",
            String::from_utf8_lossy(&install.stderr)
        );
        let output: Value = serde_json::from_slice(&install.stdout).unwrap();
        assert_eq!(output["installed_events"].as_array().unwrap().len(), 2);
    }

    let written = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        written
            .matches("hook claude-code --integration extra-eyes --event UserPromptSubmit")
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
}

#[test]
fn install_pi_writes_project_extension_idempotently() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();
    let expected_extension = project
        .canonicalize()
        .unwrap()
        .join(".pi/extensions/extra-eyes.ts");

    for _ in 0..2 {
        let install = Command::new(bin("eyes"))
            .args(["install", "pi", "--project"])
            .arg(&project)
            .args(["--eyes-bin"])
            .arg(&eyes_bin)
            .args(["--json"])
            .output()
            .unwrap();
        assert!(
            install.status.success(),
            "{}",
            String::from_utf8_lossy(&install.stderr)
        );
        let output: Value = serde_json::from_slice(&install.stdout).unwrap();
        assert_eq!(
            output["extension_path"].as_str().unwrap(),
            expected_extension.to_str().unwrap()
        );
        assert_eq!(
            output["eyes_bin"].as_str().unwrap(),
            eyes_bin.to_str().unwrap()
        );
    }

    let written = fs::read_to_string(project.join(".pi/extensions/extra-eyes.ts")).unwrap();
    assert!(written.contains("pi.on(\"input\""));
    assert!(written.contains("pi.on(\"session_shutdown\""));
    assert!(written.contains("\"feed\""));
    assert!(written.contains("\"hook\""));
    assert!(written.contains("\"fetch\""));
    assert!(written.contains("event.source === \"extension\""));
    assert!(written.contains("ctx.sessionManager.getSessionId()"));
    assert!(written.contains(&eyes_bin.display().to_string()));
}

#[test]
fn hook_claude_code_is_silent_on_malformed_payload_and_daemon_down() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let output = run_claude_code_hook_cli(&project, &runtime, "UserPromptSubmit", "not json");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let payload = r#"{"session_id":"claude-down","prompt":"daemon is down"}"#;
    let output = run_claude_code_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn hook_claude_code_records_prompt_fetches_messages_and_commits_cursor() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "claude-visible",
            "--watcher",
            "claude-test",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );

    let payload = r#"{"session_id":"claude-s1","hook_event_name":"UserPromptSubmit","prompt":"fix the installer","timestamp_ms":42}"#;
    let first_started = Instant::now();
    let first = run_claude_code_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        first_started.elapsed() < Duration::from_secs(1),
        "claude-code hook fetch took {:?}",
        first_started.elapsed()
    );
    let rendered = String::from_utf8(first.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("<extra-eyes-messages>"));
    assert!(additional_context.contains("watcher=\"claude-test\""));
    assert!(additional_context.contains("\"text\": \"claude-visible\""));

    let second = run_claude_code_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.stdout.is_empty());

    let conversation = fs::read_to_string(project.join(".eyes/state/conversation.jsonl")).unwrap();
    assert!(conversation.contains("\"harness\":\"claude-code\""));
    assert!(conversation.contains("\"role\":\"user\""));
    assert!(conversation.contains("\"text\":\"fix the installer\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_codex_records_prompt_fetches_messages_and_commits_cursor() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "codex-visible",
            "--watcher",
            "codex-test",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );

    let payload = r#"{"session_id":"codex-s1","prompt":"fix the installer","timestamp_ms":42}"#;
    let first_started = Instant::now();
    let first = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        first_started.elapsed() < Duration::from_secs(1),
        "codex hook fetch took {:?}",
        first_started.elapsed()
    );
    let rendered = String::from_utf8(first.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("<extra-eyes-messages>"));
    assert!(additional_context.contains("watcher=\"codex-test\""));
    assert!(additional_context.contains("\"text\": \"codex-visible\""));

    let second = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.stdout.is_empty());

    let conversation = fs::read_to_string(project.join(".eyes/state/conversation.jsonl")).unwrap();
    assert!(conversation.contains("\"harness\":\"codex\""));
    assert!(conversation.contains("\"role\":\"user\""));
    assert!(conversation.contains("\"text\":\"fix the installer\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_recovers_queued_message_after_daemon_restart() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "restart-visible",
            "--watcher",
            "restart",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );
    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());

    let mut restarted = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut restarted);
    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "restart-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(String::from_utf8(fetched.stdout)
        .unwrap()
        .contains("\"text\": \"restart-visible\""));

    stop_daemon(&project, &runtime);
    assert!(restarted.wait().unwrap().success());
}

#[test]
fn eyes_watch_runs_in_non_git_project_with_file_only_context() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    let capture = temp.path().join("non-git-envelope.json");
    let script = temp.path().join("non-git-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat > "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"non-git-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "watcher",
        &[&script, &capture],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    fs::write(project.join("src/lib.rs"), "pub fn seed() {}\n").unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--debounce-ms",
            "50",
            "--max-ticks",
            "1",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let keep_editing = Arc::new(AtomicBool::new(true));
    let editor_project = project.clone();
    let editor_running = Arc::clone(&keep_editing);
    let editor = thread::spawn(move || {
        for index in 0..40 {
            if !editor_running.load(Ordering::Relaxed) {
                return;
            }
            fs::write(
                editor_project.join("src/lib.rs"),
                format!("pub fn no_git_{index}() {{}}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
        }
    });
    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(envelope["context"]["diff"], "");
    assert_eq!(envelope["context"]["files"].as_array().unwrap().len(), 0);
    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "non-git-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(String::from_utf8(fetched.stdout)
        .unwrap()
        .contains("\"text\": \"non-git-ok\""));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_marks_watcher_origin_and_deduplicates_session() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "check auth before commit",
            "--watcher",
            "harold",
            "--severity",
            "warn",
            "--json",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "{}",
        String::from_utf8_lossy(&send.stderr)
    );
    let sent: Value = serde_json::from_slice(&send.stdout).unwrap();
    let message_id = sent["message_id"].as_u64().unwrap();

    let fetch_started = Instant::now();
    let first_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:s1:hook",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        first_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&first_fetch.stderr)
    );
    assert!(
        fetch_started.elapsed() < Duration::from_secs(1),
        "hook fetch took {:?}",
        fetch_started.elapsed()
    );
    let rendered = String::from_utf8(first_fetch.stdout).unwrap();
    assert!(rendered.contains("<extra-eyes-messages>"));
    assert!(rendered.contains(&format!("<extra-eyes-message id=\"{message_id}\"")));
    assert!(rendered.contains("watcher=\"harold\""));
    assert!(rendered.contains("severity=\"warn\""));
    assert!(rendered.contains("\"text\": \"check auth before commit\""));

    let second_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:s1:hook",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        second_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&second_fetch.stderr)
    );
    assert!(second_fetch.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_budgets_output_without_dropping_deferred_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "budget",
            "severity": "info",
            "text": "x".repeat(DEFAULT_HOOK_OUTPUT_BUDGET_BYTES + 16_000)
        }),
    );
    let second_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "budget",
            "severity": "info",
            "text": "second-visible"
        }),
    );

    let first_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "budget-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        first_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&first_fetch.stderr)
    );
    let first_rendered = String::from_utf8(first_fetch.stdout).unwrap();
    assert!(first_rendered.len() <= DEFAULT_HOOK_OUTPUT_BUDGET_BYTES);
    assert!(first_rendered.contains(&format!("id=\"{first_id}\"")));
    assert!(first_rendered.contains("truncated=\"true\""));
    assert!(!first_rendered.contains(&format!("id=\"{second_id}\"")));
    assert!(!first_rendered.contains("second-visible"));

    let second_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "budget-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        second_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&second_fetch.stderr)
    );
    let second_rendered = String::from_utf8(second_fetch.stdout).unwrap();
    assert!(second_rendered.contains(&format!("id=\"{second_id}\"")));
    assert!(second_rendered.contains("second-visible"));

    let third_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "budget-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        third_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&third_fetch.stderr)
    );
    assert!(third_fetch.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

fn assert_signal_cleanup(signal: libc::c_int) {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = status
        .get("socket_path")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    let pid_path = std::path::Path::new(&socket_path)
        .parent()
        .unwrap()
        .join("eyesd.pid.json");

    let pid = status.get("pid").and_then(Value::as_u64).unwrap() as libc::pid_t;
    let signal_result = unsafe { libc::kill(pid, signal) };
    assert_eq!(signal_result, 0);

    assert!(daemon.wait().unwrap().success());
    assert!(!std::path::Path::new(&socket_path).exists());
    assert!(!pid_path.exists());
}

#[test]
fn status_exits_nonzero_when_daemon_is_absent() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let output = Command::new(bin("eyesd"))
        .args(["status", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no eyesd daemon is running"));
}

#[test]
fn resolves_project_profile_from_cli() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();
    fs::write(
        watchers.join("general.toml"),
        r#"
name = "general"
default = true
prompt = "Watch carefully."
harness = "pi"
model = "anthropic/claude-opus-4-6"
"#,
    )
    .unwrap();

    let output = Command::new(bin("eyes"))
        .args(["profile", "resolve", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", temp.path().join("runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source"], "project");
    assert_eq!(json["profile"]["name"], "general");
    assert_eq!(json["profile"]["harness"], "pi");
}

fn wait_for_status(
    project: &std::path::Path,
    runtime: &std::path::Path,
    daemon: &mut Child,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut stream) = daemon.stderr.take() {
                let _ = stream.read_to_string(&mut stderr);
            }
            panic!("daemon exited early with status {status}: {stderr}");
        }
        let output = Command::new(bin("eyesd"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).unwrap();
        }
        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become ready: {last_stderr}");
}

fn spawn_daemon(project: &std::path::Path, runtime: &std::path::Path) -> Child {
    Command::new(bin("eyesd"))
        .args(["start", "--foreground", "--project"])
        .arg(project)
        .env("EXTRA_EYES_RUNTIME_DIR", runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn socket_path_from_status(status: &Value) -> std::path::PathBuf {
    status
        .get("socket_path")
        .and_then(Value::as_str)
        .unwrap()
        .into()
}

fn stop_daemon(project: &std::path::Path, runtime: &std::path::Path) {
    let stop = Command::new(bin("eyesd"))
        .args(["stop", "--project"])
        .arg(project)
        .env("EXTRA_EYES_RUNTIME_DIR", runtime)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

fn wait_until_not_running(project: &std::path::Path, runtime: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        let status = Command::new(bin("eyesd"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if !status.status.success()
            && String::from_utf8_lossy(&status.stderr).contains("no eyesd daemon is running")
        {
            return;
        }
        last_stdout = String::from_utf8_lossy(&status.stdout).to_string();
        last_stderr = String::from_utf8_lossy(&status.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not stop; stdout={last_stdout:?} stderr={last_stderr:?}");
}

fn run_codex_hook_cli(
    project: &std::path::Path,
    runtime: &std::path::Path,
    event: &str,
    payload: &str,
) -> std::process::Output {
    let mut child = Command::new(bin("eyes"))
        .args(["hook", "codex", "--event", event, "--project"])
        .arg(project)
        .env("EXTRA_EYES_RUNTIME_DIR", runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_claude_code_hook_cli(
    project: &std::path::Path,
    runtime: &std::path::Path,
    event: &str,
    payload: &str,
) -> std::process::Output {
    let mut child = Command::new(bin("eyes"))
        .args(["hook", "claude-code", "--event", event, "--project"])
        .arg(project)
        .env("EXTRA_EYES_RUNTIME_DIR", runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn enqueue(socket_path: &std::path::Path, channel: &str, payload: Value) -> u64 {
    match send_request(
        socket_path,
        &Request::EnqueueMessage {
            protocol: PROTOCOL_VERSION,
            channel: channel.to_owned(),
            payload,
        },
    )
    .unwrap()
    {
        Response::MessageEnqueued { message_id, .. } => message_id,
        other => panic!("unexpected enqueue response: {other:?}"),
    }
}

fn fetch(
    socket_path: &std::path::Path,
    channel: &str,
    cursor_key: &str,
    limit: Option<u32>,
) -> Vec<extra_eyes::ipc::IpcMessage> {
    match send_request(
        socket_path,
        &Request::FetchMessages {
            protocol: PROTOCOL_VERSION,
            channel: channel.to_owned(),
            cursor_key: cursor_key.to_owned(),
            limit,
        },
    )
    .unwrap()
    {
        Response::Messages { messages, .. } => messages,
        other => panic!("unexpected fetch response: {other:?}"),
    }
}

fn commit(
    socket_path: &std::path::Path,
    channel: &str,
    cursor_key: &str,
    through_message_id: u64,
) -> u64 {
    match send_request(
        socket_path,
        &Request::CommitCursor {
            protocol: PROTOCOL_VERSION,
            channel: channel.to_owned(),
            cursor_key: cursor_key.to_owned(),
            through_message_id,
        },
    )
    .unwrap()
    {
        Response::CursorCommitted {
            last_message_id, ..
        } => last_message_id,
        other => panic!("unexpected commit response: {other:?}"),
    }
}

fn run_watcher(
    socket_path: &std::path::Path,
    profile: &str,
    tick_id: &str,
    context: WatcherContext,
) -> Response {
    send_request(
        socket_path,
        &Request::RunWatcher {
            protocol: PROTOCOL_VERSION,
            profile: Some(profile.to_owned()),
            tick_id: tick_id.to_owned(),
            context,
        },
    )
    .unwrap()
}

fn daemon_ping(socket_path: &std::path::Path) -> bool {
    matches!(
        send_request(
            socket_path,
            &Request::Ping {
                protocol: PROTOCOL_VERSION,
            },
        ),
        Ok(Response::Pong { .. })
    )
}

fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).unwrap();
}

fn write_raw_profile(
    watchers_dir: &std::path::Path,
    name: &str,
    command: &[&std::path::Path],
    timeout_ms: Option<u64>,
    cost_limit_units: Option<u64>,
    env: Option<(&str, &str)>,
) {
    write_raw_profile_with_default(
        watchers_dir,
        name,
        command,
        timeout_ms,
        cost_limit_units,
        env,
        false,
    );
}

fn write_raw_profile_with_default(
    watchers_dir: &std::path::Path,
    name: &str,
    command: &[&std::path::Path],
    timeout_ms: Option<u64>,
    cost_limit_units: Option<u64>,
    env: Option<(&str, &str)>,
    default: bool,
) {
    let command_values = command
        .iter()
        .map(|path| toml_string(path.to_str().unwrap()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut profile = format!(
        r#"name = {name}
default = {default}
prompt = {prompt}
harness = "raw"
model = "fixture"
[settings]
command = [{command_values}]
"#,
        name = toml_string(name),
        default = default,
        prompt = toml_string(&format!("Prompt for {name}")),
    );
    if let Some(timeout_ms) = timeout_ms {
        profile.push_str(&format!("timeout_ms = {timeout_ms}\n"));
    }
    if let Some(cost_limit_units) = cost_limit_units {
        profile.push_str(&format!("cost_limit_units = {cost_limit_units}\n"));
    }
    if let Some((key, value)) = env {
        profile.push_str("[settings.env]\n");
        profile.push_str(&format!("{} = {}\n", key, toml_string(value)));
    }
    fs::write(watchers_dir.join(format!("{name}.toml")), profile).unwrap();
}

fn git(project: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
