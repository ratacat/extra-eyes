use std::collections::BTreeSet;
use std::env;
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
use extra_eyes::ipc::{
    send_request, write_frame, IpcMessage, Request, Response, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use extra_eyes::paths::ProjectPaths;
use extra_eyes::watcher::WatcherContext;

const FIXTURE_WATCHER_TIMEOUT_MS: u64 = 5_000;

fn bin(name: &str) -> &'static str {
    match name {
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

    let stop = Command::new(bin("eyes"))
        .args(["daemon", "stop", "--project"])
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
fn daemon_uses_shared_git_root_for_repo_subdirectories() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    let watched = repo.join("watched");
    let other = repo.join("other");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&watched).unwrap();
    fs::create_dir_all(&other).unwrap();
    git(&repo, &["init", "-q"]);

    let mut daemon = spawn_daemon(&watched, &runtime);
    let status = wait_for_status(&watched, &runtime, &mut daemon);

    assert_eq!(
        status.get("project_root").and_then(Value::as_str).unwrap(),
        repo.canonicalize().unwrap().to_str().unwrap()
    );

    let hook = run_codex_hook_cli(
        &other,
        &runtime,
        "UserPromptSubmit",
        r#"{"session_id":"outside-sibling","prompt":"outside sibling prompt","timestamp_ms":42}"#,
    );
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );

    let repo_conversation =
        fs::read_to_string(repo.join(".eyes/state/conversation.jsonl")).unwrap_or_default();
    assert!(
        repo_conversation.contains("outside sibling prompt"),
        "{repo_conversation}"
    );
    assert!(
        !watched.join(".eyes/state/conversation.jsonl").exists(),
        "watched subdirectory created split state"
    );
    assert!(
        !other.join(".eyes/state/conversation.jsonl").exists(),
        "other subdirectory created split state"
    );

    stop_daemon(&watched, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn daemon_start_defaults_to_detached_background_process() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let start = Command::new(bin("eyes"))
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

    let status = Command::new(bin("eyes"))
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
fn detached_daemon_start_refuses_second_start_for_same_project() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let first = Command::new(bin("eyes"))
        .args(["start", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = Command::new(bin("eyes"))
        .args(["start", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already"));

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn status_reports_single_daemon_for_requested_project() {
    let temp = TempDir::new().unwrap();
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    let mut daemon_a = spawn_daemon(&project_a, &runtime);
    let status_a = wait_for_status(&project_a, &runtime, &mut daemon_a);
    let pid_a = status_a["pid"].as_u64().unwrap().to_string();
    let project_b_root = project_b
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let human = Command::new(bin("eyes"))
        .args(["status", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let rendered = String::from_utf8(human.stdout).unwrap();
    assert!(rendered.contains("Extra Eyes daemon"));
    assert!(rendered.contains("Running daemon"));
    assert!(rendered.lines().any(|line| line.contains(&pid_a)
        && line.contains(&project_b_root)
        && line.trim_start().starts_with('*')));

    let json = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let parsed: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(parsed["status"], "running");
    assert_eq!(parsed["current_pid"].as_u64().unwrap().to_string(), pid_a);
    assert_eq!(parsed["project_root"], project_b_root);
    let daemons = parsed["daemons"].as_array().unwrap();
    assert_eq!(daemons.len(), 1);
    assert_eq!(
        daemons
            .iter()
            .filter(|daemon| daemon["current"] == true)
            .count(),
        1
    );

    stop_daemon(&project_a, &runtime);
    assert!(daemon_a.wait().unwrap().success());
}

#[test]
fn status_reports_watch_liveness_separately_from_daemon_bus() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let daemon_status = wait_for_status(&project, &runtime, &mut daemon);
    assert_eq!(daemon_status["status"], "running");
    assert_eq!(daemon_status["watch"]["active"], false);

    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "50",
            "--debounce-ms",
            "25",
            "--idle-timeout-ms",
            "700",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let active_status = wait_for_watch_status(&project, &runtime, &mut watch, true);
    assert_eq!(active_status["status"], "running");
    assert_eq!(active_status["watch"]["active"], true);
    assert_eq!(active_status["watch"]["pid"], watch.id());
    assert!(active_status["watch"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|profile| profile == "general"));

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let inactive_status = wait_for_watch_status(&project, &runtime, &mut daemon, false);
    assert_eq!(inactive_status["status"], "running");
    assert_eq!(inactive_status["watch"]["active"], false);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn stop_tears_down_watch_loop_and_daemon_bus() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let daemon_status = wait_for_status(&project, &runtime, &mut daemon);
    assert_eq!(daemon_status["status"], "running");

    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "50",
            "--debounce-ms",
            "25",
            "--idle-timeout-ms",
            "5000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let active_status = wait_for_watch_status(&project, &runtime, &mut watch, true);
    assert_eq!(active_status["watch"]["active"], true);

    // `eyes stop` now tears down the whole stack: the daemon SIGTERMs the watch
    // loop and then shuts the bus down.
    let stop = Command::new(bin("eyes"))
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

    // Both the watch loop and the daemon process exit.
    let _watch_status = wait_for_child_exit(&mut watch, "project watch");
    assert!(daemon.wait().unwrap().success());

    // The bus is gone: a fresh status reports not running.
    let after = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    let after_json: Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(after_json["status"], "not_running");
}

#[test]
fn gc_reaps_dead_pidfile_and_leaves_clean_runtime() {
    let temp = TempDir::new().unwrap();
    let runtime = temp.path().join("runtime");
    let daemon_dir = runtime.join("daemon");
    fs::create_dir_all(&daemon_dir).unwrap();

    // Use a dead pid: spawn `true`, reap it, reuse its now-exited pid.
    let mut dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();

    let pid_path = daemon_dir.join("eyesd.pid.json");
    let socket_path = daemon_dir.join("eyesd.sock");
    fs::write(
        &pid_path,
        serde_json::json!({
            "pid": dead_pid,
            "project_root": temp.path().join("project").to_str().unwrap(),
            "project_hash": "deadbeef",
            "started_at_ms": 1_u64
        })
        .to_string(),
    )
    .unwrap();
    fs::write(&socket_path, "").unwrap();

    // Dry run reports the dead daemon but leaves the pidfile in place.
    let dry = Command::new(bin("eyes"))
        .args(["gc", "--dry-run", "--json"])
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["reaped"][0]["reason"], "dead");
    assert!(pid_path.exists(), "dry run must not remove the pidfile");

    // Real run reaps it and removes the pidfile + stale socket.
    let gc = Command::new(bin("eyes"))
        .args(["gc", "--json"])
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let gc_json: Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(gc_json["reaped"][0]["reason"], "dead");
    assert!(!pid_path.exists(), "pidfile should be removed");
    assert!(!socket_path.exists(), "stale socket should be removed");

    // A second run finds nothing left to reap.
    let again = Command::new(bin("eyes"))
        .args(["gc", "--json"])
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    let again_json: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again_json["reaped"].as_array().unwrap().len(), 0);
}

#[test]
fn detached_start_uses_one_daemon_process_for_multiple_projects() {
    let temp = TempDir::new().unwrap();
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    let start_a = Command::new(bin("eyes"))
        .args(["start", "--json", "--project"])
        .arg(&project_a)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        start_a.status.success(),
        "{}",
        String::from_utf8_lossy(&start_a.stderr)
    );
    let started_a: Value = serde_json::from_slice(&start_a.stdout).unwrap();

    let status_b = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    let parsed_b = if status_b.status.success() {
        Some(serde_json::from_slice::<Value>(&status_b.stdout).unwrap())
    } else {
        None
    };

    let start_b = Command::new(bin("eyes"))
        .args(["start", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();

    stop_daemon(&project_a, &runtime);
    wait_until_not_running(&project_a, &runtime);

    let parsed_b = parsed_b.unwrap_or_else(|| {
        panic!(
            "status for project-b failed: {}",
            String::from_utf8_lossy(&status_b.stderr)
        )
    });
    assert_eq!(parsed_b["status"], "running");
    assert_eq!(parsed_b["pid"], started_a["pid"]);
    assert_eq!(parsed_b["socket_path"], started_a["socket_path"]);
    assert_eq!(
        parsed_b["project_root"],
        project_b.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(!start_b.status.success());
    assert!(String::from_utf8_lossy(&start_b.stderr).contains("already"));
}

#[test]
fn tick_routes_to_requested_project_through_single_daemon() {
    let temp = TempDir::new().unwrap();
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project_a).unwrap();
    let watchers_b = project_b.join(".eyes/watchers");
    fs::create_dir_all(&watchers_b).unwrap();

    let script = temp.path().join("project-b-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"project-b-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers_b,
        "project-b-watcher",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let start_a = Command::new(bin("eyes"))
        .args(["start", "--json", "--project"])
        .arg(&project_a)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        start_a.status.success(),
        "{}",
        String::from_utf8_lossy(&start_a.stderr)
    );

    let tick_b = Command::new(bin("eyes"))
        .args(["tick", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();

    stop_daemon(&project_a, &runtime);
    wait_until_not_running(&project_a, &runtime);

    assert!(
        tick_b.status.success(),
        "{}",
        String::from_utf8_lossy(&tick_b.stderr)
    );
    assert!(String::from_utf8_lossy(&tick_b.stdout).contains("project-b-ok"));
    assert!(
        fs::read_to_string(project_b.join(".eyes/state/messages.jsonl"))
            .unwrap()
            .contains("project-b-ok")
    );
    assert!(!project_a.join(".eyes/state/messages.jsonl").exists());
}

#[test]
fn daemon_restart_replaces_running_daemon() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let first_status = wait_for_status(&project, &runtime, &mut daemon);
    let first_pid = first_status["pid"].as_u64().unwrap();

    let restart = Command::new(bin("eyes"))
        .args(["restart", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(daemon.wait().unwrap().success());

    let restarted: Value = serde_json::from_slice(&restart.stdout).unwrap();
    assert_eq!(restarted["stopped_existing"], true);
    assert_ne!(restarted["pid"].as_u64().unwrap(), first_pid);

    let status = Command::new(bin("eyes"))
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
    assert_eq!(status_json["pid"], restarted["pid"]);

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn daemon_restart_starts_when_daemon_is_absent() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let restart = Command::new(bin("eyes"))
        .args(["restart", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );

    let restarted: Value = serde_json::from_slice(&restart.stdout).unwrap();
    assert_eq!(restarted["stopped_existing"], false);
    assert!(restarted["pid"].as_u64().unwrap() > 0);

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

    let second = Command::new(bin("eyes"))
        .args(["daemon", "foreground", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already"));

    let stop = Command::new(bin("eyes"))
        .args(["daemon", "stop", "--project"])
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
fn single_daemon_stays_running_when_one_project_root_disappears() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let other_project = temp.path().join("other-project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&other_project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let pid_path = socket_path.parent().unwrap().join("eyesd.pid.json");
    assert!(socket_path.exists());
    assert!(pid_path.exists());

    fs::remove_dir_all(&project).unwrap();

    let status = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&other_project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["status"], "running");
    assert!(socket_path.exists());
    assert!(pid_path.exists());

    stop_daemon(&other_project, &runtime);
    assert!(daemon.wait().unwrap().success());
    assert!(!socket_path.exists());
    assert!(!pid_path.exists());
}

#[test]
fn daemon_evicts_missing_project_runtime_when_serving_another_project() {
    let temp = TempDir::new().unwrap();
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();

    let mut daemon = spawn_daemon(&project_a, &runtime);
    let status_a = wait_for_status(&project_a, &runtime, &mut daemon);
    let project_a_root = project_a
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let project_b_root = project_b
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(status_a["loaded_projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|root| root == &project_a_root));

    let status_b = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        status_b.status.success(),
        "{}",
        String::from_utf8_lossy(&status_b.stderr)
    );
    let status_b: Value = serde_json::from_slice(&status_b.stdout).unwrap();
    assert!(status_b["loaded_projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|root| root == &project_a_root));
    assert!(status_b["loaded_projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|root| root == &project_b_root));

    fs::remove_dir_all(&project_a).unwrap();

    let after_removal = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project_b)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        after_removal.status.success(),
        "{}",
        String::from_utf8_lossy(&after_removal.stderr)
    );
    let after_removal: Value = serde_json::from_slice(&after_removal.stdout).unwrap();
    let loaded = after_removal["loaded_projects"].as_array().unwrap();
    assert!(!loaded.iter().any(|root| root == &project_a_root));
    assert!(loaded.iter().any(|root| root == &project_b_root));
    assert_eq!(after_removal["status"], "running");

    stop_daemon(&project_b, &runtime);
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

    let stop = Command::new(bin("eyes"))
        .args(["daemon", "stop", "--project"])
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
fn eyes_watch_restarts_daemon_without_current_build_id() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let identity = ProjectIdentity::from_root(project.clone()).unwrap();
    let paths = ProjectPaths::from_identity_with_runtime_base(identity, runtime.clone()).unwrap();
    paths.ensure().unwrap();
    let listener = UnixListener::bind(paths.socket_path()).unwrap();
    let socket_path = paths.socket_path().to_path_buf();
    let project_root = paths.identity().root_string().to_owned();
    let project_hash = paths.identity().hash().to_owned();
    let state_dir = paths.state_dir().display().to_string();
    let stale = thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let request: Request = extra_eyes::ipc::read_frame(&mut stream).unwrap();
            match request {
                Request::Status { .. } | Request::Project { .. } => {
                    write_frame(
                        &mut stream,
                        &serde_json::json!({
                            "type": "status",
                            "protocol": PROTOCOL_VERSION,
                            "pid": 12345,
                            "project_root": project_root,
                            "project_hash": project_hash,
                            "socket_path": socket_path.display().to_string(),
                            "state_dir": state_dir,
                        }),
                    )
                    .unwrap();
                }
                Request::Stop { .. } => {
                    write_frame(
                        &mut stream,
                        &serde_json::json!({
                            "type": "stopping",
                            "protocol": PROTOCOL_VERSION,
                        }),
                    )
                    .unwrap();
                    break;
                }
                other => panic!("unexpected fake daemon request: {other:?}"),
            }
        }
        fs::remove_file(&socket_path).unwrap();
    });

    let watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--idle-timeout-ms",
            "1",
            "--poll-ms",
            "25",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(
        watch.status.success(),
        "{}",
        String::from_utf8_lossy(&watch.stderr)
    );
    stale.join().unwrap();
    let stderr = String::from_utf8_lossy(&watch.stderr);
    assert!(stderr.contains("restarted"), "{stderr}");
    assert!(stderr.contains("stale binary"), "{stderr}");

    let status = Command::new(bin("eyes"))
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
    assert_eq!(status_json["status"], "running");
    assert_eq!(status_json["build_id"], status_json["current_build_id"]);

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
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
fn daemon_serves_conversation_traffic_while_watcher_is_running() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let marker = temp.path().join("watcher-started");
    let script = temp.path().join("slow-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
marker="$1"
printf started > "$marker"
cat >/dev/null
sleep 1
printf '%s\n' '{"v":1,"type":"message","text":"slow-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "watcher",
        &[&script, &marker],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let mut tick = Command::new(bin("eyes"))
        .args(["tick", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_path(&marker);

    let feed_started = Instant::now();
    let feed = Command::new(bin("eyes"))
        .args(["feed", "--project"])
        .arg(&project)
        .args([
            "--harness",
            "codex",
            "--event",
            "UserPromptSubmit",
            "--payload-json",
            r#"{"session_id":"fast-session","prompt":"hook should not wait","timestamp_ms":1}"#,
        ])
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        feed.status.success(),
        "{}",
        String::from_utf8_lossy(&feed.stderr)
    );
    assert!(
        feed_started.elapsed() < Duration::from_millis(500),
        "feed blocked behind watcher for {:?}",
        feed_started.elapsed()
    );

    let tick_exit = tick.wait().unwrap();
    if !tick_exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = tick.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("tick exited with {tick_exit}: {stderr}");
    }

    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let replayed = fetch(&socket_path, "hook", "fast-session", None);
    let watcher_messages = replayed
        .iter()
        .filter(|message| message.payload["kind"] != "watcher_check_in")
        .collect::<Vec<_>>();
    assert_eq!(watcher_messages.len(), 1);
    assert_eq!(watcher_messages[0].payload["text"], "slow-ok");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
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
fn targeted_messages_deliver_to_session_ids_with_colons() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let message_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "routing",
            "severity": "info",
            "text": "colon session visible",
            "target_harness": "codex",
            "target_session_id": "workspace:abc"
        }),
    );

    let messages = fetch(&socket_path, "hook", "codex:workspace:abc:hook", None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, message_id);
    assert!(fetch(&socket_path, "hook", "codex:workspace:other:hook", None).is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn targeted_messages_are_isolated_by_harness_and_session() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let codex_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "routing",
            "severity": "info",
            "text": "codex only",
            "target_harness": "codex",
            "target_session_id": "same-session"
        }),
    );
    let claude_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "routing",
            "severity": "info",
            "text": "claude only",
            "target_harness": "claude-code",
            "target_session_id": "same-session"
        }),
    );

    let codex = fetch(&socket_path, "hook", "codex:same-session:hook", None);
    assert_eq!(codex.len(), 1);
    assert_eq!(codex[0].message_id, codex_id);

    let claude = fetch(&socket_path, "hook", "claude-code:same-session:hook", None);
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0].message_id, claude_id);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn truncated_targeted_messages_preserve_source_event_routing_metadata() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let payload_for = |text_len: usize| {
        serde_json::json!({
            "watcher": "huge",
            "severity": "warning",
            "text": format!("oversized:{}", "x".repeat(text_len)),
            "target_harness": "codex",
            "target_session_id": "source-session",
            "source_event_id": 4242_u64
        })
    };
    let enqueue_len = |text_len: usize| {
        serde_json::to_vec(&Request::EnqueueMessage {
            protocol: PROTOCOL_VERSION,
            channel: "hook".to_owned(),
            payload: payload_for(text_len),
        })
        .unwrap()
        .len()
    };

    let mut low = 0;
    let mut high = MAX_FRAME_SIZE;
    while low < high {
        let mid = (low + high).div_ceil(2);
        if enqueue_len(mid) <= MAX_FRAME_SIZE {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    let payload = payload_for(low);
    let untruncated_response = Response::Messages {
        protocol: PROTOCOL_VERSION,
        channel: "hook".to_owned(),
        cursor_key: "codex:source-session:hook".to_owned(),
        last_message_id: 0,
        latest_message_id: 1,
        messages: vec![IpcMessage {
            message_id: 1,
            channel: "hook".to_owned(),
            created_at_ms: 1,
            payload: payload.clone(),
        }],
    };
    assert!(serde_json::to_vec(&untruncated_response).unwrap().len() > MAX_FRAME_SIZE);

    let message_id = enqueue(&socket_path, "hook", payload);
    let messages = fetch(&socket_path, "hook", "codex:source-session:hook", None);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, message_id);
    assert_eq!(messages[0].payload["truncated"], true);
    assert_eq!(messages[0].payload["target_harness"], "codex");
    assert_eq!(messages[0].payload["target_session_id"], "source-session");
    assert_eq!(messages[0].payload["source_event_id"], 4242);
    assert!(messages[0].payload["text"]
        .as_str()
        .unwrap()
        .contains("[extra-eyes truncated oversized queued message for hook transport]"));

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
fn watcher_check_in_is_enqueued_once_per_watcher() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first = send_request(
        &socket_path,
        &Request::EnsureWatcherCheckIn {
            protocol: PROTOCOL_VERSION,
            watcher: "security".to_owned(),
            target_harness: None,
            target_session_id: None,
        },
    )
    .unwrap();
    match first {
        Response::WatcherCheckIn {
            watcher,
            message_id: Some(1),
            ..
        } => assert_eq!(watcher, "security"),
        other => panic!("unexpected check-in response: {other:?}"),
    }

    let second = send_request(
        &socket_path,
        &Request::EnsureWatcherCheckIn {
            protocol: PROTOCOL_VERSION,
            watcher: "security".to_owned(),
            target_harness: None,
            target_session_id: None,
        },
    )
    .unwrap();
    match second {
        Response::WatcherCheckIn {
            watcher,
            message_id: Some(1),
            ..
        } => assert_eq!(watcher, "security"),
        other => panic!("unexpected check-in response: {other:?}"),
    }

    let messages = fetch(&socket_path, "hook", "check-in-session", None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload["kind"], "watcher_check_in");
    assert_eq!(messages[0].payload["watcher"], "security");
    assert!(messages[0].payload["text"]
        .as_str()
        .unwrap()
        .contains("Extra Eyes watcher `security` is connected"));

    let inbox = fs::read_to_string(project.join(".eyes/inbox.md")).unwrap();
    assert!(inbox.contains("watcher_check_in") || inbox.contains("Check-in"));

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
            after_message_id: None,
            targeted_only: false,
            include_all_targets: false,
        },
    )
    .unwrap();
    assert!(matches!(zero_limit, Response::Error { .. }));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn cursor_compare_and_commit_rejects_stale_claims() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let message_id = enqueue(&socket_path, "hook", serde_json::json!({"body":"one"}));

    let first = send_request(
        &socket_path,
        &Request::CommitCursorIfCurrent {
            protocol: PROTOCOL_VERSION,
            channel: "hook".to_owned(),
            cursor_key: "session:a".to_owned(),
            expected_last_message_id: 0,
            through_message_id: message_id,
        },
    )
    .unwrap();
    assert!(matches!(first, Response::CursorCommitted { .. }));

    let stale = send_request(
        &socket_path,
        &Request::CommitCursorIfCurrent {
            protocol: PROTOCOL_VERSION,
            channel: "hook".to_owned(),
            cursor_key: "session:a".to_owned(),
            expected_last_message_id: 0,
            through_message_id: message_id,
        },
    )
    .unwrap();
    assert!(matches!(
        stale,
        Response::CursorCommitStale {
            last_message_id,
            ..
        } if last_message_id == message_id
    ));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn cursor_commits_may_land_on_cross_channel_gaps() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    enqueue(&socket_path, "hook", serde_json::json!({"body":"before"}));
    let gap_message_id = enqueue(&socket_path, "other", serde_json::json!({"body":"gap"}));
    let after_message_id = enqueue(&socket_path, "hook", serde_json::json!({"body":"after"}));

    assert_eq!(
        commit(&socket_path, "hook", "session:gap", gap_message_id),
        gap_message_id
    );
    let messages = fetch(&socket_path, "hook", "session:gap", None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_id, after_message_id);

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
  printf '%s\n' "$EXTRA_EYES_MODEL"
  printf '%s\n' "$EXTRA_EYES_REASONING_EFFORT"
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
            repo_package: String::new(),
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
    let prompt = envelope["prompt"].as_str().unwrap();
    assert!(prompt.contains("strongest intent signal"), "{prompt}");
    assert!(
        prompt.contains("Watcher scope:\nPrompt for echo"),
        "{prompt}"
    );
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
    assert_eq!(meta_lines[4], "fixture");
    assert_eq!(meta_lines[5], "low");

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
            assert_eq!(message_ids.len(), 5);
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
    let failure_fetch = fetch(&socket_path, "hook", "nonzero-session", None);
    assert_eq!(failure_fetch.len(), 5);
    assert!(failure_fetch.iter().any(|message| message.payload["text"]
        .as_str()
        .unwrap()
        .contains("Watcher `bad` emitted malformed output")));
    assert!(failure_fetch.iter().any(|message| message.payload["text"]
        .as_str()
        .unwrap()
        .contains("Watcher `bad` emitted unsupported output")));
    assert!(failure_fetch.iter().any(|message| message.payload["text"]
        .as_str()
        .unwrap()
        .contains("Watcher `bad` exited unsuccessfully")));
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
    let stdin_blocked = temp.path().join("stdin-blocked-watcher.sh");
    write_executable(
        &stdin_blocked,
        r#"#!/bin/sh
sleep 60
"#,
    );
    write_raw_profile(
        &watchers,
        "stdin-blocked",
        &[&stdin_blocked],
        Some(50),
        Some(10),
        None,
    );
    let orphan_marker = temp.path().join("orphan-marker");
    let pipe_holder = temp.path().join("pipe-holder-watcher.sh");
    write_executable(
        &pipe_holder,
        &format!(
            r#"#!/bin/sh
cat >/dev/null
(sleep 1; printf orphan > {}) &
exit 0
"#,
            shell_quote(orphan_marker.to_str().unwrap())
        ),
    );
    write_raw_profile(
        &watchers,
        "pipe-holder",
        &[&pipe_holder],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    let escaped_holder = temp.path().join("escaped-holder-watcher.sh");
    write_executable(
        &escaped_holder,
        r#"#!/bin/sh
setsid sh -c 'sleep 2 <&0 >/dev/null 2>/dev/null' &
exit 0
"#,
    );
    write_raw_profile(
        &watchers,
        "escaped-holder",
        &[&escaped_holder],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
    let output_spam = temp.path().join("output-spam-watcher.sh");
    // Must exceed MAX_WATCHER_OUTPUT_BYTES (1 MiB) to trip output_limit_exceeded.
    write_executable(
        &output_spam,
        r#"#!/bin/sh
cat >/dev/null
dd if=/dev/zero bs=1024 count=1200 2>/dev/null | tr '\0' x
"#,
    );
    write_raw_profile(
        &watchers,
        "output-spam",
        &[&output_spam],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );
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
            message_ids,
            statuses,
            ..
        } if message_ids.is_empty() && statuses.iter().any(|status| status.outcome == "timeout")
    ));
    let timeout_fetch = fetch(&socket_path, "hook", "timeout-session", None);
    assert!(timeout_fetch.is_empty());

    let repeated_timeout = run_watcher(
        &socket_path,
        "sleepy",
        "tick-timeout-repeat",
        WatcherContext::default(),
    );
    assert!(matches!(
        repeated_timeout,
        Response::WatcherRun { message_ids, .. } if message_ids.is_empty()
    ));
    assert!(daemon_ping(&socket_path));

    let blocked_context = WatcherContext {
        diff: "x".repeat(256 * 1024),
        ..WatcherContext::default()
    };
    let started = Instant::now();
    let blocked_response = run_watcher(
        &socket_path,
        "stdin-blocked",
        "tick-stdin-blocked",
        blocked_context,
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stdin-blocked watcher took {:?}",
        started.elapsed()
    );
    assert!(matches!(
        blocked_response,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.is_empty() && statuses.iter().any(|status| status.outcome == "timeout")
    ));
    assert!(daemon_ping(&socket_path));

    let started = Instant::now();
    let pipe_holder_response = run_watcher(
        &socket_path,
        "pipe-holder",
        "tick-pipe-holder",
        WatcherContext::default(),
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "pipe-holder watcher took {:?}",
        started.elapsed()
    );
    assert!(matches!(
        pipe_holder_response,
        Response::WatcherRun {
            summary,
            ..
        } if summary.state == "quiet"
    ));
    thread::sleep(Duration::from_millis(1200));
    assert!(!orphan_marker.exists());
    assert!(daemon_ping(&socket_path));

    let escaped_context = WatcherContext {
        diff: "x".repeat(256 * 1024),
        ..WatcherContext::default()
    };
    let started = Instant::now();
    let escaped_response = run_watcher(
        &socket_path,
        "escaped-holder",
        "tick-escaped-holder",
        escaped_context,
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "escaped-holder watcher took {:?}",
        started.elapsed()
    );
    assert!(matches!(
        escaped_response,
        Response::WatcherRun {
            summary,
            ..
        } if summary.state == "quiet"
    ));
    assert!(daemon_ping(&socket_path));

    let spam_response = run_watcher(
        &socket_path,
        "output-spam",
        "tick-output-spam",
        WatcherContext::default(),
    );
    assert!(matches!(
        spam_response,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.len() == 2
            && statuses
            .iter()
            .any(|status| status.outcome == "malformed_stdout")
            && statuses
            .iter()
            .any(|status| status.outcome == "output_limit_exceeded")
    ));

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
fn watcher_failure_diagnostics_statuses_and_inbox_are_route_aware() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("route-api-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"status","severity":"error","outcome":"api_failure","text":"rate_limit"}'
"#,
    );
    write_raw_profile(
        &watchers,
        "api",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first = run_watcher_for_route(
        &socket_path,
        "api",
        "tick-route-a",
        WatcherContext::default(),
        Some("codex"),
        Some("session-a"),
        Some(1),
    );
    assert!(matches!(
        first,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.len() == 1
            && statuses.iter().any(|status| status.outcome == "api_failure")
    ));

    let second = run_watcher_for_route(
        &socket_path,
        "api",
        "tick-route-b",
        WatcherContext::default(),
        Some("codex"),
        Some("session-b"),
        Some(2),
    );
    assert!(matches!(
        second,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.len() == 1
            && statuses.iter().any(|status| status.outcome == "api_failure")
    ));

    let third = run_watcher_for_route(
        &socket_path,
        "api",
        "tick-route-a-repeat",
        WatcherContext::default(),
        Some("codex"),
        Some("session-a"),
        Some(3),
    );
    assert!(matches!(
        third,
        Response::WatcherRun { message_ids, .. } if message_ids.is_empty()
    ));

    let session_a = fetch(&socket_path, "hook", "codex:session-a:hook", None);
    assert_eq!(session_a.len(), 1);
    assert_eq!(session_a[0].payload["target_harness"], "codex");
    assert_eq!(session_a[0].payload["target_session_id"], "session-a");
    let session_b = fetch(&socket_path, "hook", "codex:session-b:hook", None);
    assert_eq!(session_b.len(), 1);
    assert_eq!(session_b[0].payload["target_harness"], "codex");
    assert_eq!(session_b[0].payload["target_session_id"], "session-b");

    let statuses = send_request(
        &socket_path,
        &Request::WatcherStatuses {
            protocol: PROTOCOL_VERSION,
        },
    )
    .unwrap();
    let statuses = match statuses {
        Response::WatcherStatuses { watchers, .. } => serde_json::to_value(watchers).unwrap(),
        other => panic!("unexpected statuses response: {other:?}"),
    };
    let statuses = statuses.as_array().unwrap();
    assert!(statuses.iter().any(|status| {
        status["watcher"] == "api" && status["target_session_id"] == "session-a"
    }));
    assert!(statuses.iter().any(|status| {
        status["watcher"] == "api" && status["target_session_id"] == "session-b"
    }));

    let inbox = fs::read_to_string(project.join(".eyes/inbox.md")).unwrap();
    assert!(inbox.contains("- target: `codex:session-a`"), "{inbox}");
    assert!(inbox.contains("- target: `codex:session-b`"), "{inbox}");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_codex_cli_failure_is_reported_once_until_recovery() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let counter = temp.path().join("codex-counter");
    let script = temp.path().join("codex-watcher.sh");
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
    printf '%s\n' '{"v":1,"type":"status","severity":"warning","outcome":"codex_cli_failed","text":"debug watcher could not query Codex CLI","details":{"stderr_excerpt":"sandbox denied"}}'
    ;;
  *)
    printf '%s\n' '{"v":1,"type":"message","text":"recovered"}'
    ;;
esac
"#,
    );
    write_raw_profile(
        &watchers,
        "debug",
        &[&script, &counter],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let first = run_watcher(
        &socket_path,
        "debug",
        "tick-codex-1",
        WatcherContext::default(),
    );
    assert!(matches!(
        first,
        Response::WatcherRun {
            message_ids,
            statuses,
            ..
        } if message_ids.len() == 1
            && statuses.iter().any(|status| status.outcome == "codex_cli_failed")
    ));
    let first_fetch = fetch(&socket_path, "hook", "codex-failure-session", None);
    assert_eq!(first_fetch.len(), 1);
    let first_text = first_fetch[0].payload["text"].as_str().unwrap();
    assert!(
        first_text.contains("could not run Codex CLI"),
        "{first_text}"
    );
    assert!(
        first_text.contains("debug watcher could not query Codex CLI"),
        "{first_text}"
    );
    assert_eq!(first_fetch[0].payload["severity"], "warning");
    commit(
        &socket_path,
        "hook",
        "codex-failure-session",
        first_fetch[0].message_id,
    );

    let second = run_watcher(
        &socket_path,
        "debug",
        "tick-codex-2",
        WatcherContext::default(),
    );
    assert!(matches!(
        second,
        Response::WatcherRun { message_ids, .. } if message_ids.is_empty()
    ));
    assert!(fetch(&socket_path, "hook", "codex-failure-session", None).is_empty());

    let recovered = run_watcher(
        &socket_path,
        "debug",
        "tick-codex-3",
        WatcherContext::default(),
    );
    assert!(matches!(
        recovered,
        Response::WatcherRun { message_ids, .. } if message_ids.len() == 1
    ));
    let recovered_fetch = fetch(&socket_path, "hook", "codex-failure-session", None);
    assert_eq!(recovered_fetch.len(), 1);
    commit(
        &socket_path,
        "hook",
        "codex-failure-session",
        recovered_fetch[0].message_id,
    );

    let fourth = run_watcher(
        &socket_path,
        "debug",
        "tick-codex-4",
        WatcherContext::default(),
    );
    assert!(matches!(
        fourth,
        Response::WatcherRun { message_ids, .. } if message_ids.len() == 1
    ));
    let fourth_fetch = fetch(&socket_path, "hook", "codex-failure-session", None);
    assert_eq!(fourth_fetch.len(), 1);
    let fourth_text = fourth_fetch[0].payload["text"].as_str().unwrap();
    assert!(
        fourth_text.contains("could not run Codex CLI"),
        "{fourth_text}"
    );

    let status_rows = fs::read_to_string(project.join(".eyes/state/watcher-status.jsonl")).unwrap();
    assert!(status_rows.contains("sandbox denied"), "{status_rows}");

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

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-manual", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", temp.path().join("home"))
        .env("CODEX_HOME", temp.path().join("codex-home"))
        .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let tick_stderr = String::from_utf8_lossy(&tick.stderr);
    assert!(tick_stderr.contains("eyes daemon"), "{tick_stderr}");
    assert!(tick_stderr.contains("started"), "{tick_stderr}");
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
        .env("CODEX_HOME", temp.path().join("codex-home"))
        .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
        .output()
        .unwrap();
    assert!(fetched.status.success());
    assert!(String::from_utf8(fetched.stdout)
        .unwrap()
        .contains("tick-ok"));

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn eyes_tick_context_includes_staged_and_untracked_paths() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();
    git(&project, &["init", "-q"]);
    fs::write(
        project.join("src/staged.rs"),
        "pub fn staged_context() -> bool { true }\n",
    )
    .unwrap();
    fs::write(
        project.join("src/untracked.rs"),
        "pub fn untracked_context() -> bool { true }\n",
    )
    .unwrap();
    git(&project, &["add", "src/staged.rs"]);

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

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-context", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let envelope: Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    let files = envelope["context"]["files"].as_array().unwrap();
    assert!(
        files.iter().any(|file| file == "src/staged.rs"),
        "{files:?}"
    );
    assert!(
        files.iter().any(|file| file == "src/untracked.rs"),
        "{files:?}"
    );
    assert!(envelope["context"]["diff"]
        .as_str()
        .unwrap()
        .contains("staged_context"));

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
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
            repo_package: String::new(),
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
    assert_eq!(bundled_json["profile"]["harness"], "codex");
    assert_eq!(bundled_json["profile"]["model"], "gpt-5.5");
    assert_eq!(bundled_json["profile"]["reasoning_effort"], "high");
    assert_eq!(bundled_json["profile"]["settings"]["timeout_ms"], 180000);
    assert!(!bundled_json["profile"]["settings"]
        .as_object()
        .unwrap()
        .contains_key("command"));

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
fn human_output_color_can_be_forced_or_disabled() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();

    let forced = Command::new(bin("eyes"))
        .args(["--color", "always", "profile", "resolve", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_HOME", &home)
        .output()
        .unwrap();
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let forced_stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(forced_stdout.contains("\x1b["), "{forced_stdout}");

    let disabled = Command::new(bin("eyes"))
        .args(["--color", "never", "profile", "resolve", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_HOME", &home)
        .output()
        .unwrap();
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    let disabled_stdout = String::from_utf8_lossy(&disabled.stdout);
    assert!(!disabled_stdout.contains("\x1b["), "{disabled_stdout}");
    assert_eq!(
        disabled_stdout,
        "eyes profile ready name=general  source=bundled\n"
    );
}

#[test]
fn bundled_default_profile_runs_via_codex_cli_shim() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let home = temp.path().join("home");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    let codex = bin_dir.join("codex");
    let args_capture = temp.path().join("codex-args.txt");
    let stdin_capture = temp.path().join("codex-stdin.txt");
    write_executable(
        &codex,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > {args_capture}
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    out="$1"
  fi
  shift || true
done
cat > {stdin_capture}
printf '%s\n' '{{"v":1,"type":"message","severity":"info","text":"bundled-ok"}}' > "$out"
"#,
            args_capture = shell_quote(args_capture.to_str().unwrap()),
            stdin_capture = shell_quote(stdin_capture.to_str().unwrap())
        ),
    );

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-bundled", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", &home)
        .env("PATH", prepend_path(&bin_dir))
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let tick_stdout = String::from_utf8_lossy(&tick.stdout);
    assert!(tick_stdout.contains("eyes general"), "{tick_stdout}");
    assert!(tick_stdout.contains("info"), "{tick_stdout}");
    assert!(tick_stdout.contains("bundled-ok"), "{tick_stdout}");
    let codex_args = fs::read_to_string(&args_capture).unwrap();
    assert!(codex_args.contains("gpt-5.5"), "{codex_args}");
    assert!(
        codex_args.contains("model_reasoning_effort=\"high\""),
        "{codex_args}"
    );
    assert!(!codex_args.contains("xhigh"), "{codex_args}");
    assert!(!codex_args.contains("model_service_tier"), "{codex_args}");
    let codex_stdin = fs::read_to_string(&stdin_capture).unwrap();
    assert!(
        codex_stdin.contains("Use the JSON `prompt` field as your scoped watcher brief."),
        "{codex_stdin}"
    );
    assert!(
        codex_stdin.contains("strongest intent signal"),
        "{codex_stdin}"
    );
    assert!(
        codex_stdin.contains("Watcher scope:\\nWatch the working agent"),
        "{codex_stdin}"
    );

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "bundled-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", &home)
        .output()
        .unwrap();
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert!(String::from_utf8_lossy(&fetched.stdout).contains("bundled-ok"));

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn codex_profile_uses_profile_model_reasoning_and_timeout() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let home = temp.path().join("home");
    let watchers = project.join(".eyes/watchers");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&watchers).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    fs::write(
        watchers.join("reviewer.toml"),
        r#"
name = "reviewer"
default = true
prompt = "Review carefully."
harness = "codex"
model = "gpt-5.5-mini"
reasoning_effort = "medium"
[settings]
timeout_ms = 180000
context_budget_bytes = 2048
"#,
    )
    .unwrap();

    let codex = bin_dir.join("codex");
    let args_capture = temp.path().join("codex-args.txt");
    let stdin_capture = temp.path().join("codex-stdin.txt");
    write_executable(
        &codex,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > {args_capture}
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    out="$1"
  fi
  shift || true
done
cat > {stdin_capture}
printf '%s\n' '{{"v":1,"type":"message","severity":"warning","text":"codex-profile-ok"}}' > "$out"
"#,
            args_capture = shell_quote(args_capture.to_str().unwrap()),
            stdin_capture = shell_quote(stdin_capture.to_str().unwrap())
        ),
    );

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-codex-profile", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", &home)
        .env("PATH", prepend_path(&bin_dir))
        .output()
        .unwrap();

    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let tick_stdout = String::from_utf8_lossy(&tick.stdout);
    assert!(tick_stdout.contains("codex-profile-ok"), "{tick_stdout}");
    let codex_args = fs::read_to_string(&args_capture).unwrap();
    assert!(codex_args.contains("gpt-5.5-mini"), "{codex_args}");
    assert!(
        codex_args.contains("model_reasoning_effort=\"medium\""),
        "{codex_args}"
    );
    let codex_stdin = fs::read_to_string(&stdin_capture).unwrap();
    assert!(
        codex_stdin.contains("Watcher scope:\\nReview carefully."),
        "{codex_stdin}"
    );

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn bundled_default_reports_non_json_model_output() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let home = temp.path().join("home");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    let codex = bin_dir.join("codex");
    write_executable(
        &codex,
        r#"#!/bin/sh
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    out="$1"
  fi
  shift || true
done
cat >/dev/null
printf '%s\n' 'assistant prose' > "$out"
printf '%s\n' '{"v":1,"type":"message","severity":"info","text":"bundled-ok"}' >> "$out"
"#,
    );

    let tick = Command::new(bin("eyes"))
        .args(["tick", "--tick-id", "tick-bundled", "--project"])
        .arg(&project)
        .arg("--json")
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", &home)
        .env("PATH", prepend_path(&bin_dir))
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    let tick_json: Value = serde_json::from_slice(&tick.stdout).unwrap();
    assert_eq!(tick_json["message_ids"].as_array().unwrap().len(), 2);
    assert!(tick_json["messages"][0]["text"]
        .as_str()
        .unwrap()
        .contains("bundled-ok"));
    assert!(tick_json["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .any(|status| status["outcome"] == "malformed_stdout"));

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "bundled-malformed-session",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", &home)
        .output()
        .unwrap();
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let fetched_stdout = String::from_utf8_lossy(&fetched.stdout);
    assert!(fetched_stdout.contains("bundled-ok"), "{fetched_stdout}");
    assert!(
        fetched_stdout.contains("Watcher `general` emitted malformed output"),
        "{fetched_stdout}"
    );

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
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
        .contains("watch-ok"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_prints_each_sent_message() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("talker-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","severity":"warning","text":"first warning line one\nline two stays on its own line and this long tail stays visible after the old one hundred eighty byte compact rendering point abcdefghijklmnopqrstuvwxyz abcdefghijklmnopqrstuvwxyz abcdefghijklmnopqrstuvwxyz abcdefghijklmnopqrstuvwxyz","refs":[{"path":"src/lib.rs","line":1}]}'
printf '%s\n' '{"v":1,"type":"message","severity":"error","text":"second-error"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "talker",
        &[&script],
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
            "25",
            "--max-ticks",
            "1",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::piped())
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
                format!("pub fn repeated_{index}() {{}}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
        }
    });

    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    let mut stdout = String::new();
    if let Some(mut stream) = watch.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let lines = stdout.lines().collect::<Vec<_>>();
    assert!(
        lines[0].starts_with("talker -> Check-in: watcher `talker`"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n\ntalker -> first warning line one\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n  line two stays on its own line"),
        "{stdout}"
    );
    assert!(
        stdout.contains("old one hundred eighty byte compact rendering point"),
        "{stdout}"
    );
    assert!(stdout.contains("\n  [src/lib.rs:1]\n\n"), "{stdout}");
    assert!(stdout.contains("\n\ntalker -> second-error"), "{stdout}");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_prints_timeout_diagnostics() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("sleepy-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
sleep 5
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "sleepy",
        &[&script],
        Some(50),
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
            "25",
            "--max-ticks",
            "1",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::piped())
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
                format!("pub fn status_only_{index}() {{}}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(150));
        }
    });

    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    let mut stdout = String::new();
    if let Some(mut stream) = watch.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }
    assert!(stdout.contains("Check-in: watcher `sleepy`"), "{stdout}");
    assert!(stdout.contains("timeout: timed out after 50ms"), "{stdout}");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_prints_idle_queue_messages_from_daemon_queue() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();
    let script = temp.path().join("quiet-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "quiet",
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
            "--idle-timeout-ms",
            "400",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_watch_scoped_check_in(&project);
    let send = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "manual-visible-in-watch",
            "--watcher",
            "manual",
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

    let exit = watch.wait().unwrap();
    let mut stdout = String::new();
    if let Some(mut stream) = watch.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }
    assert!(stdout.contains("Check-in: watcher `quiet`"), "{stdout}");
    assert!(stdout.contains("manual-visible-in-watch"), "{stdout}");

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_starts_from_scoped_check_in_without_replaying_old_backlog() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("quiet-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "quiet",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let first_watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--idle-timeout-ms",
            "250",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        first_watch.status.success(),
        "{}",
        String::from_utf8_lossy(&first_watch.stderr)
    );
    assert!(String::from_utf8_lossy(&first_watch.stdout).contains("Check-in: watcher `quiet`"));

    let stale = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "stale-before-new-watch",
            "--watcher",
            "quiet",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );

    let second_watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--idle-timeout-ms",
            "250",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        second_watch.status.success(),
        "{}",
        String::from_utf8_lossy(&second_watch.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second_watch.stdout);
    assert!(
        second_stdout.contains("Check-in: watcher `quiet`"),
        "{second_stdout}"
    );
    assert!(
        !second_stdout.contains("stale-before-new-watch"),
        "{second_stdout}"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_prints_queued_status_diagnostics() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("api-failure-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"status","severity":"error","outcome":"api_failure","text":"rate_limit"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "api",
        &[&script],
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
            "25",
            "--max-ticks",
            "1",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::piped())
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
                format!("pub fn changed_{index}() {{}}\n"),
            )
            .unwrap();
            thread::sleep(Duration::from_millis(50));
        }
    });

    let exit = watch.wait().unwrap();
    keep_editing.store(false, Ordering::Relaxed);
    editor.join().unwrap();
    let mut stdout = String::new();
    if let Some(mut stream) = watch.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }
    assert!(stdout.contains("Check-in: watcher `api`"), "{stdout}");
    assert!(
        stdout.contains("Watcher `api` reported API failure: rate_limit"),
        "{stdout}"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_autostarts_daemon_before_idle_exit() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "25",
            "--idle-timeout-ms",
            "50",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", temp.path().join("home"))
        .output()
        .unwrap();

    assert!(
        watch.status.success(),
        "{}",
        String::from_utf8_lossy(&watch.stderr)
    );
    let watch_stdout = String::from_utf8(watch.stdout).unwrap();
    assert!(
        watch_stdout.contains("Check-in: watcher `general`"),
        "{watch_stdout}"
    );
    let watch_stderr = String::from_utf8_lossy(&watch.stderr);
    assert!(watch_stderr.contains("eyes daemon"), "{watch_stderr}");
    assert!(watch_stderr.contains("started"), "{watch_stderr}");
    assert!(watch_stderr.contains("eyes watch"), "{watch_stderr}");
    assert!(
        watch_stderr.contains("watcher profile general [bundled, default profile]"),
        "{watch_stderr}"
    );
    assert!(
        watch_stderr.contains("triggers file, conversation"),
        "{watch_stderr}"
    );
    assert!(
        watch_stderr.contains("messages general -> <watcher message>"),
        "{watch_stderr}"
    );
    assert!(watch_stderr.contains("Hook coverage"), "{watch_stderr}");
    assert!(watch_stderr.contains("Codex"), "{watch_stderr}");
    assert!(watch_stderr.contains("Claude Code"), "{watch_stderr}");
    assert!(watch_stderr.contains("pi"), "{watch_stderr}");

    let status = Command::new(bin("eyes"))
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
    let json: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        json["project_root"],
        project.canonicalize().unwrap().display().to_string()
    );

    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
}

#[test]
fn bare_eyes_defaults_to_watch_and_autostarts_daemon() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut watch = Command::new(bin("eyes"))
        .current_dir(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("EXTRA_EYES_HOME", temp.path().join("home"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let _status = wait_for_bare_eyes_status(&project, &runtime, &mut watch);

    watch.kill().unwrap();
    let _ = watch.wait().unwrap();
    stop_daemon(&project, &runtime);
    wait_until_not_running(&project, &runtime);
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
    assert_eq!(rendered.matches("fast-ok").count(), 2);
    assert_eq!(rendered.matches("slow-ok").count(), 1);
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
    let rendered = String::from_utf8(fetched.stdout).unwrap();
    assert!(rendered.contains("kind=\"watcher_check_in\""));
    assert!(rendered.contains("watcher=\"watcher\""));
    assert!(!rendered.contains("should-not-run"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_runs_when_conversation_events_arrive() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
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
            "25",
            "--max-ticks",
            "1",
            "--idle-timeout-ms",
            "2000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(75));
    for prompt in ["@eyes first", "@eyes hello"] {
        let payload = serde_json::json!({
            "session_id": "message-session",
            "prompt": prompt,
            "timestamp_ms": 1,
        });
        let feed = Command::new(bin("eyes"))
            .args(["feed", "--project"])
            .arg(&project)
            .args([
                "--harness",
                "codex",
                "--event",
                "UserPromptSubmit",
                "--payload-json",
                &payload.to_string(),
            ])
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            feed.status.success(),
            "{}",
            String::from_utf8_lossy(&feed.stderr)
        );
        thread::sleep(Duration::from_millis(100));
    }

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let envelope = fs::read_to_string(&capture).unwrap();
    assert!(envelope.contains("@eyes"), "{envelope}");

    let fetched = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:message-session:hook",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(fetched.status.success());
    let rendered = String::from_utf8(fetched.stdout).unwrap();
    assert!(rendered.contains("conversation-ok"), "{rendered}");

    let other_session = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:other-session:hook",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(other_session.status.success());
    let other_rendered = String::from_utf8(other_session.stdout).unwrap();
    assert!(
        !other_rendered.contains("conversation-ok"),
        "{other_rendered}"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_routes_coalesced_conversation_events_to_each_session() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("coalesced-conversation-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"coalesced-conversation-ok"}'
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
            "50",
            "--debounce-ms",
            "25",
            "--max-ticks",
            "2",
            "--idle-timeout-ms",
            "2000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(75));
    for session_id in ["session-a", "session-b"] {
        let payload = serde_json::json!({
            "session_id": session_id,
            "prompt": format!("@eyes from {session_id}"),
            "timestamp_ms": 1,
        });
        let feed = Command::new(bin("eyes"))
            .args(["feed", "--project"])
            .arg(&project)
            .args([
                "--harness",
                "codex",
                "--event",
                "UserPromptSubmit",
                "--payload-json",
                &payload.to_string(),
            ])
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            feed.status.success(),
            "{}",
            String::from_utf8_lossy(&feed.stderr)
        );
    }

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    for session_id in ["session-a", "session-b"] {
        let fetched = Command::new(bin("eyes"))
            .args([
                "hook",
                "fetch",
                "--cursor-key",
                &format!("codex:{session_id}:hook"),
                "--project",
            ])
            .arg(&project)
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            fetched.status.success(),
            "{}",
            String::from_utf8_lossy(&fetched.stderr)
        );
        let rendered = String::from_utf8(fetched.stdout).unwrap();
        assert!(rendered.contains("coalesced-conversation-ok"), "{rendered}");
    }

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_scopes_watcher_context_to_triggering_session() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let capture = temp.path().join("scoped-context.jsonl");
    let script = temp.path().join("scoped-context-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat >> "$capture"
printf '\n' >> "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"scoped-context-ok"}'
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

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "50",
            "--debounce-ms",
            "25",
            "--max-ticks",
            "2",
            "--idle-timeout-ms",
            "2000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(75));
    for (session_id, prompt) in [
        ("session-a", "session a prompt"),
        ("session-b", "session b prompt"),
    ] {
        let payload = serde_json::json!({
            "session_id": session_id,
            "prompt": prompt,
            "timestamp_ms": 1,
        });
        let feed = Command::new(bin("eyes"))
            .args(["feed", "--project"])
            .arg(&project)
            .args([
                "--harness",
                "codex",
                "--event",
                "UserPromptSubmit",
                "--payload-json",
                &payload.to_string(),
            ])
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            feed.status.success(),
            "{}",
            String::from_utf8_lossy(&feed.stderr)
        );
    }

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let envelopes = fs::read_to_string(&capture).unwrap();
    let mut seen = BTreeSet::new();
    for line in envelopes.lines().filter(|line| !line.trim().is_empty()) {
        let envelope: Value = serde_json::from_str(line).unwrap();
        let conversation = envelope["context"]["conversation"].as_array().unwrap();
        assert_eq!(conversation.len(), 1, "{line}");
        seen.insert(conversation[0]["session_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        seen,
        BTreeSet::from(["session-a".to_owned(), "session-b".to_owned()])
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_bounds_watcher_context_to_source_conversation_event() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let capture = temp.path().join("event-bounded-context.jsonl");
    let script = temp.path().join("event-bounded-context-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
capture="$1"
cat >> "$capture"
printf '\n' >> "$capture"
printf '%s\n' '{"v":1,"type":"message","text":"event-bounded-context-ok"}'
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

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);
    let mut watch = Command::new(bin("eyes"))
        .args([
            "watch",
            "--poll-ms",
            "500",
            "--debounce-ms",
            "25",
            "--max-ticks",
            "2",
            "--idle-timeout-ms",
            "3000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    for prompt in ["first prompt", "second prompt"] {
        let payload = serde_json::json!({
            "session_id": "same-session",
            "prompt": prompt,
            "timestamp_ms": 1,
        });
        let feed = Command::new(bin("eyes"))
            .args(["feed", "--project"])
            .arg(&project)
            .args([
                "--harness",
                "codex",
                "--event",
                "UserPromptSubmit",
                "--payload-json",
                &payload.to_string(),
            ])
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            feed.status.success(),
            "{}",
            String::from_utf8_lossy(&feed.stderr)
        );
    }

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let envelopes = fs::read_to_string(&capture).unwrap();
    let mut contexts = Vec::new();
    for line in envelopes.lines().filter(|line| !line.trim().is_empty()) {
        let envelope: Value = serde_json::from_str(line).unwrap();
        let prompts = envelope["context"]["conversation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["text"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        contexts.push(prompts);
    }

    assert_eq!(
        contexts,
        vec![
            vec!["first prompt".to_owned()],
            vec!["first prompt".to_owned(), "second prompt".to_owned()]
        ]
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_tags_session_messages_with_source_conversation_event() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();

    let script = temp.path().join("source-event-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"v":1,"type":"message","text":"source-event-ok"}'
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
            "50",
            "--debounce-ms",
            "25",
            "--max-ticks",
            "1",
            "--idle-timeout-ms",
            "2000",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(75));
    let payload = serde_json::json!({
        "session_id": "source-session",
        "prompt": "@eyes answer this exact prompt",
        "timestamp_ms": 1,
    });
    let feed = Command::new(bin("eyes"))
        .args(["feed", "--json", "--project"])
        .arg(&project)
        .args([
            "--harness",
            "codex",
            "--event",
            "UserPromptSubmit",
            "--payload-json",
            &payload.to_string(),
        ])
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        feed.status.success(),
        "{}",
        String::from_utf8_lossy(&feed.stderr)
    );
    let recorded: Value = serde_json::from_slice(&feed.stdout).unwrap();
    let event_id = recorded["event_id"].as_u64().unwrap();

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let messages = fs::read_to_string(project.join(".eyes/state/messages.jsonl")).unwrap();
    let message = messages
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["payload"]["text"] == "source-event-ok")
        .expect("watcher message not recorded");
    assert_eq!(message["payload"]["target_session_id"], "source-session");
    assert_eq!(message["payload"]["target_harness"], "codex");
    assert_eq!(message["payload"]["source_event_id"], event_id);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn eyes_watch_idle_timeout_resets_after_file_changes() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&watchers).unwrap();

    let run_log = temp.path().join("idle-runs.log");
    let script = temp.path().join("idle-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
run_log="$1"
cat >/dev/null
printf '%s\n' "$EXTRA_EYES_TICK_ID" >> "$run_log"
printf '%s\n' '{"v":1,"type":"message","text":"idle-ok"}'
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "watcher",
        &[&script, &run_log],
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
            "25",
            "--idle-timeout-ms",
            "1000",
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

    thread::sleep(Duration::from_millis(200));
    fs::write(project.join("src/lib.rs"), "pub fn first() {}\n").unwrap();
    let first_log = wait_for_log_line_count(&run_log, 1, Duration::from_secs(2));
    assert_eq!(first_log.lines().count(), 1, "{first_log}");
    fs::write(project.join("src/lib.rs"), "pub fn second() {}\n").unwrap();

    let exit = watch.wait().unwrap();
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = watch.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        panic!("watch exited with {exit}: {stderr}");
    }

    let log = fs::read_to_string(&run_log).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");

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
    assert_eq!(output["installed_events"].as_array().unwrap().len(), 4);
    assert_eq!(output["trust_entries"].as_array().unwrap().len(), 4);
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
    assert_eq!(output["trust_entries"].as_array().unwrap().len(), 4);

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
    assert!(!written.contains("echo existing:"));
    assert_eq!(written.matches("async = false").count(), 4);
    assert_eq!(written.matches("trusted_hash = \"sha256:").count(), 4);
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
    assert_eq!(output["installed_events"].as_array().unwrap().len(), 4);

    let written = fs::read_to_string(&config).unwrap();
    assert!(written.contains("hook codex --integration extra-eyes --event UserPromptSubmit"));
    assert!(written.contains("hook codex --integration extra-eyes --event PreToolUse"));
    assert_eq!(written.matches("trusted_hash = \"sha256:").count(), 4);
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
        assert_eq!(output["installed_events"].as_array().unwrap().len(), 3);
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
        assert_eq!(output["installed_events"].as_array().unwrap().len(), 3);
    }

    let written = fs::read_to_string(project.join(".pi/extensions/extra-eyes.ts")).unwrap();
    assert!(written.contains("pi.on(\"input\""));
    assert!(written.contains("pi.on(\"tool_call\""));
    assert!(written.contains("pi.on(\"session_shutdown\""));
    assert!(written.contains("pi.sendMessage"));
    assert!(written.contains("\"feed\""));
    assert!(written.contains("\"hook\""));
    assert!(written.contains("\"fetch\""));
    assert!(written.contains("\"--limit\""));
    assert!(written.contains("event.source === \"extension\""));
    assert!(written.contains("ctx.sessionManager.getSessionId()"));
    assert!(written.contains(&eyes_bin.display().to_string()));
}

#[test]
fn status_reports_harness_hook_coverage() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let codex_home = temp.path().join("codex-home");
    let claude_config = temp.path().join("claude");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&claude_config).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();

    let codex_config = codex_home.join("config.toml");
    let install_codex = Command::new(bin("eyes"))
        .args(["install", "codex", "--config"])
        .arg(&codex_config)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .output()
        .unwrap();
    assert!(
        install_codex.status.success(),
        "{}",
        String::from_utf8_lossy(&install_codex.stderr)
    );

    let install_pi = Command::new(bin("eyes"))
        .args(["install", "pi", "--project"])
        .arg(&project)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .output()
        .unwrap();
    assert!(
        install_pi.status.success(),
        "{}",
        String::from_utf8_lossy(&install_pi.stderr)
    );

    let human = Command::new(bin("eyes"))
        .args(["--color", "never", "status", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", &claude_config)
        .output()
        .unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let rendered = String::from_utf8(human.stdout).unwrap();
    assert!(rendered.contains("Hook coverage"), "{rendered}");
    assert!(rendered.contains("✓ Codex"), "{rendered}");
    assert!(rendered.contains("PreToolUse"), "{rendered}");
    assert!(rendered.contains("× Claude Code"), "{rendered}");
    assert!(rendered.contains("✓ pi"), "{rendered}");
    assert!(rendered.contains("tool_call"), "{rendered}");

    let json = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", &claude_config)
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let parsed: Value = serde_json::from_slice(&json.stdout).unwrap();
    let coverage = parsed["hook_coverage"].as_array().unwrap();
    assert_eq!(coverage.len(), 3);
    assert!(coverage
        .iter()
        .any(|entry| entry["harness"] == "Codex" && entry["installed"] == true));
    assert!(coverage
        .iter()
        .any(|entry| entry["harness"] == "Claude Code" && entry["installed"] == false));
}

#[test]
fn doctor_reports_daemon_hooks_and_duplicate_counts() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let codex_home = temp.path().join("codex-home");
    let claude_config = temp.path().join("claude");
    let eyes_bin = temp.path().join("bin/eyes");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&claude_config).unwrap();
    fs::create_dir_all(eyes_bin.parent().unwrap()).unwrap();

    let codex_config = codex_home.join("config.toml");
    let claude_settings = claude_config.join("settings.json");
    let install_codex = Command::new(bin("eyes"))
        .args(["install", "codex", "--config"])
        .arg(&codex_config)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .output()
        .unwrap();
    assert!(
        install_codex.status.success(),
        "{}",
        String::from_utf8_lossy(&install_codex.stderr)
    );
    let install_claude = Command::new(bin("eyes"))
        .args(["install", "claude-code", "--settings"])
        .arg(&claude_settings)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .output()
        .unwrap();
    assert!(
        install_claude.status.success(),
        "{}",
        String::from_utf8_lossy(&install_claude.stderr)
    );
    let install_pi = Command::new(bin("eyes"))
        .args(["install", "pi", "--project"])
        .arg(&project)
        .args(["--eyes-bin"])
        .arg(&eyes_bin)
        .output()
        .unwrap();
    assert!(
        install_pi.status.success(),
        "{}",
        String::from_utf8_lossy(&install_pi.stderr)
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let clean = Command::new(bin("eyes"))
        .args(["doctor", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", &claude_config)
        .output()
        .unwrap();
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean_json: Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(clean_json["status"], "warning");
    assert!(clean_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "watch active" && check["status"] == "warning" }));
    assert!(clean_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "Codex hook counts" && check["status"] == "ok" }));
    assert!(clean_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "Claude Code hook counts" && check["status"] == "ok" }));

    let mut settings: Value =
        serde_json::from_str(&fs::read_to_string(&claude_settings).unwrap()).unwrap();
    let hooks = settings["hooks"]["UserPromptSubmit"][0]["hooks"]
        .as_array_mut()
        .unwrap();
    let duplicate = hooks[0].clone();
    hooks.push(duplicate);
    fs::write(
        &claude_settings,
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let duplicate = Command::new(bin("eyes"))
        .args(["doctor", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", &claude_config)
        .output()
        .unwrap();
    assert!(
        duplicate.status.success(),
        "{}",
        String::from_utf8_lossy(&duplicate.stderr)
    );
    let duplicate_json: Value = serde_json::from_slice(&duplicate.stdout).unwrap();
    assert_eq!(duplicate_json["status"], "error");
    let claude_count = duplicate_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Claude Code hook counts")
        .unwrap();
    assert_eq!(claude_count["status"], "error");
    assert!(claude_count["details"]
        .as_str()
        .unwrap()
        .contains("UserPromptSubmit has 2 Extra Eyes command(s)"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
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
    assert!(additional_context.contains("claude-visible"));

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
fn hook_claude_code_infers_canonical_project_from_payload_cwd() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let nested = project.join("a/b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&nested).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .arg(&project)
        .output()
        .unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let payload = serde_json::json!({
        "session_id": "nested-claude",
        "cwd": project,
        "hook_event_name": "UserPromptSubmit",
        "prompt": "nested claude hook should use repo root",
        "timestamp_ms": 42
    });
    let output = run_claude_code_hook_cli_from_cwd_without_project(
        &nested,
        &runtime,
        "UserPromptSubmit",
        &payload.to_string(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let root_conversation =
        fs::read_to_string(project.join(".eyes/state/conversation.jsonl")).unwrap();
    assert!(
        root_conversation.contains("nested claude hook should use repo root"),
        "{root_conversation}"
    );
    assert!(
        !nested.join(".eyes/state/conversation.jsonl").exists(),
        "nested hook created split state"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_claude_code_pre_tool_use_flushes_pending_messages_without_waiting() {
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
            "claude-tool-visible",
            "--watcher",
            "claude-tool",
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

    let payload = r#"{"session_id":"claude-tool-s1","tool_name":"Bash","tool_input":{"command":"cargo test"},"timestamp_ms":42}"#;
    let started = Instant::now();
    let first = run_claude_code_hook_cli(&project, &runtime, "PreToolUse", payload);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "claude-code PreToolUse hook fetch took {:?}",
        started.elapsed()
    );
    let rendered = String::from_utf8(first.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("claude-tool-visible"));

    let second = run_claude_code_hook_cli(&project, &runtime, "PreToolUse", payload);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_codex_records_prompt_and_delivers_compact_on_pre_tool_use() {
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
        "codex UserPromptSubmit hook took {:?}",
        first_started.elapsed()
    );
    let prompt_stdout = String::from_utf8(first.stdout).unwrap();
    assert!(prompt_stdout.is_empty(), "{prompt_stdout}");

    let pre_tool_payload = r#"{"session_id":"codex-s1","tool_name":"Bash","tool_input":{"command":"cargo test"},"timestamp_ms":43}"#;
    let delivery = run_codex_hook_cli(&project, &runtime, "PreToolUse", pre_tool_payload);
    assert!(
        delivery.status.success(),
        "{}",
        String::from_utf8_lossy(&delivery.stderr)
    );
    let delivery_stdout = String::from_utf8(delivery.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&delivery_stdout).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("Extra Eyes notes"));
    assert!(additional_context.contains("- codex-test info: codex-visible"));
    assert!(!additional_context.contains("<extra-eyes"));

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
fn hook_codex_infers_canonical_project_from_payload_cwd() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let nested = project.join("a/b");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&nested).unwrap();
    fs::write(project.join(".gitignore"), "target\n").unwrap();
    Command::new("git")
        .args(["init"])
        .arg(&project)
        .output()
        .unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let payload = serde_json::json!({
        "session_id": "nested-codex",
        "cwd": project,
        "prompt": "nested hook should use repo root",
        "timestamp_ms": 42
    });
    let output = run_codex_hook_cli_from_cwd_without_project(
        &nested,
        &runtime,
        "UserPromptSubmit",
        &payload.to_string(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let root_conversation =
        fs::read_to_string(project.join(".eyes/state/conversation.jsonl")).unwrap();
    assert!(
        root_conversation.contains("nested hook should use repo root"),
        "{root_conversation}"
    );
    assert!(
        !nested.join(".eyes/state/conversation.jsonl").exists(),
        "nested hook created split state"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_codex_pre_tool_use_flushes_compact_pending_messages_without_waiting() {
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
            "codex-tool-visible",
            "--watcher",
            "codex-tool",
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

    let payload = r#"{"session_id":"codex-tool-s1","tool_name":"Bash","tool_input":{"command":"cargo test"},"timestamp_ms":42}"#;
    let started = Instant::now();
    let first = run_codex_hook_cli(&project, &runtime, "PreToolUse", payload);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "codex PreToolUse hook fetch took {:?}",
        started.elapsed()
    );
    let first_stdout = String::from_utf8(first.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&first_stdout).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("- codex-tool info: codex-tool-visible"));
    assert!(!additional_context.contains("<extra-eyes"));

    let second = run_codex_hook_cli(&project, &runtime, "PreToolUse", payload);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn codex_session_start_does_not_drop_existing_check_in() {
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
            "watcher connected",
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

    let session_start = run_codex_hook_cli(
        &project,
        &runtime,
        "SessionStart",
        r#"{"session_id":"codex-new","timestamp_ms":42}"#,
    );
    assert!(
        session_start.status.success(),
        "{}",
        String::from_utf8_lossy(&session_start.stderr)
    );
    let session_start_stdout = String::from_utf8(session_start.stdout).unwrap();
    assert!(session_start_stdout.is_empty(), "{session_start_stdout}");

    let prompt = run_codex_hook_cli(
        &project,
        &runtime,
        "UserPromptSubmit",
        r#"{"session_id":"codex-new","prompt":"continue","timestamp_ms":43}"#,
    );
    assert!(
        prompt.status.success(),
        "{}",
        String::from_utf8_lossy(&prompt.stderr)
    );
    let prompt_stdout = String::from_utf8(prompt.stdout).unwrap();
    assert!(prompt_stdout.is_empty(), "{prompt_stdout}");

    let pre_tool = run_codex_hook_cli(
        &project,
        &runtime,
        "PreToolUse",
        r#"{"session_id":"codex-new","tool_name":"Bash","tool_input":{"command":"pwd"},"timestamp_ms":44}"#,
    );
    assert!(
        pre_tool.status.success(),
        "{}",
        String::from_utf8_lossy(&pre_tool.stderr)
    );
    let pre_tool_stdout = String::from_utf8(pre_tool.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&pre_tool_stdout).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "PreToolUse"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("- codex-test info: watcher connected"));
    assert!(!additional_context.contains("<extra-eyes"));

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn watcher_pulse_is_compact_and_quiet_status_does_not_inject_hook_context() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    let watchers = project.join(".eyes/watchers");
    fs::create_dir_all(&watchers).unwrap();
    let script = temp.path().join("quiet-watcher.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
cat >/dev/null
"#,
    );
    write_raw_profile_with_default(
        &watchers,
        "quiet",
        &[&script],
        Some(FIXTURE_WATCHER_TIMEOUT_MS),
        Some(10),
        None,
        true,
    );

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let pulse = Command::new(bin("eyes"))
        .args([
            "watcher",
            "run",
            "quiet",
            "--tick-id",
            "tick-quiet",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        pulse.status.success(),
        "{}",
        String::from_utf8_lossy(&pulse.stderr)
    );
    let pulse_stdout = String::from_utf8(pulse.stdout).unwrap();
    assert!(pulse_stdout.contains("eyes quiet"), "{pulse_stdout}");
    assert!(pulse_stdout.contains("quiet"), "{pulse_stdout}");
    assert!(pulse_stdout.contains("no issues"), "{pulse_stdout}");

    let payload = r#"{"session_id":"codex-status","prompt":"continue","timestamp_ms":42}"#;
    let hook = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let hook_stdout = String::from_utf8(hook.stdout).unwrap();
    assert!(hook_stdout.is_empty(), "{hook_stdout}");

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
        .contains("restart-visible"));

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
    let files = envelope["context"]["files"].as_array().unwrap();
    assert!(files.iter().any(|file| file == "src/lib.rs"), "{files:?}");
    assert!(envelope["context"]["diff"]
        .as_str()
        .unwrap()
        .contains("no_git_"));
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
        .contains("non-git-ok"));

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
    assert!(rendered.contains("check auth before commit"));

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
fn hook_fetch_does_not_commit_cursor_when_stdout_write_fails() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);
    let message_id = enqueue(
        &socket_path,
        "hook",
        serde_json::json!({
            "watcher": "harold",
            "severity": "warning",
            "text": "do not lose me",
        }),
    );

    let mut broken_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "broken-stdout",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(broken_fetch.stdout.take());
    let broken = broken_fetch.wait_with_output().unwrap();
    assert!(!broken.status.success());

    let pending = fetch(&socket_path, "hook", "broken-stdout", None);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message_id, message_id);

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_can_wait_for_late_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let sender_project = project.clone();
    let sender_runtime = runtime.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let send = Command::new(bin("eyes"))
            .args([
                "message",
                "send",
                "late watcher note",
                "--watcher",
                "harold",
                "--severity",
                "info",
                "--project",
            ])
            .arg(&sender_project)
            .env("EXTRA_EYES_RUNTIME_DIR", &sender_runtime)
            .output()
            .unwrap();
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
    });

    let fetch_started = Instant::now();
    let fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:s1:hook",
            "--wait-ms",
            "2000",
            "--wait-poll-ms",
            "25",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    assert!(
        fetch_started.elapsed() < Duration::from_secs(1),
        "waited too long for delayed message: {:?}",
        fetch_started.elapsed()
    );
    let rendered = String::from_utf8(fetch.stdout).unwrap();
    assert!(rendered.contains("late watcher note"), "{rendered}");
    sender.join().unwrap();

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_fresh_wait_ignores_stale_pending_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let stale = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "stale manual note",
            "--watcher",
            "harold",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );

    let sender_project = project.clone();
    let sender_runtime = runtime.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let send = Command::new(bin("eyes"))
            .args([
                "message",
                "send",
                "fresh manual note",
                "--watcher",
                "harold",
                "--project",
            ])
            .arg(&sender_project)
            .env("EXTRA_EYES_RUNTIME_DIR", &sender_runtime)
            .output()
            .unwrap();
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
    });

    let fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "fresh-manual",
            "--wait-ms",
            "2000",
            "--wait-poll-ms",
            "25",
            "--fresh",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let rendered = String::from_utf8(fetch.stdout).unwrap();
    assert!(!rendered.contains("stale manual note"), "{rendered}");
    assert!(rendered.contains("fresh manual note"), "{rendered}");
    sender.join().unwrap();

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_fresh_wait_skips_stale_backlog_beyond_fetch_limit() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    for index in 1..=3 {
        let stale = Command::new(bin("eyes"))
            .args([
                "message",
                "send",
                &format!("stale manual note {index}"),
                "--watcher",
                "harold",
                "--project",
            ])
            .arg(&project)
            .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            stale.status.success(),
            "{}",
            String::from_utf8_lossy(&stale.stderr)
        );
    }

    let sender_project = project.clone();
    let sender_runtime = runtime.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let send = Command::new(bin("eyes"))
            .args([
                "message",
                "send",
                "fresh manual note after backlog",
                "--watcher",
                "harold",
                "--project",
            ])
            .arg(&sender_project)
            .env("EXTRA_EYES_RUNTIME_DIR", &sender_runtime)
            .output()
            .unwrap();
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
    });

    let fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "fresh-backlog",
            "--limit",
            "2",
            "--wait-ms",
            "2000",
            "--wait-poll-ms",
            "25",
            "--fresh",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let rendered = String::from_utf8(fetch.stdout).unwrap();
    assert!(!rendered.contains("stale manual note"), "{rendered}");
    assert!(
        rendered.contains("fresh manual note after backlog"),
        "{rendered}"
    );
    sender.join().unwrap();

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn hook_fetch_fresh_timeout_does_not_commit_stale_messages() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let _status = wait_for_status(&project, &runtime, &mut daemon);

    let stale = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "stale survives fresh timeout",
            "--watcher",
            "harold",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );

    let fresh_timeout = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "fresh-timeout",
            "--wait-ms",
            "100",
            "--wait-poll-ms",
            "25",
            "--fresh",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        fresh_timeout.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh_timeout.stderr)
    );
    assert!(fresh_timeout.stdout.is_empty());

    let normal_fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "fresh-timeout",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        normal_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&normal_fetch.stderr)
    );
    let rendered = String::from_utf8(normal_fetch.stdout).unwrap();
    assert!(
        rendered.contains("stale survives fresh timeout"),
        "{rendered}"
    );

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn codex_user_prompt_submit_injects_direct_eyes_reply() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let sender_socket_path = socket_path.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        enqueue(
            &sender_socket_path,
            "hook",
            serde_json::json!({
                "watcher": "debug",
                "severity": "info",
                "text": "@eyes: pong",
                "target_harness": "codex",
                "target_session_id": "codex-direct"
            }),
        );
    });

    let payload = r#"{"session_id":"codex-direct","prompt":"@eyes say pong","timestamp_ms":42}"#;
    let output = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sender.join().unwrap();

    let rendered = String::from_utf8(output.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional_context.contains("- debug info: @eyes: pong"));
    assert!(!additional_context.contains("<extra-eyes"));

    let pre_tool_payload = r#"{"session_id":"codex-direct","tool_name":"Bash","tool_input":{"command":"pwd"},"timestamp_ms":43}"#;
    let pre_tool = run_codex_hook_cli(&project, &runtime, "PreToolUse", pre_tool_payload);
    assert!(
        pre_tool.status.success(),
        "{}",
        String::from_utf8_lossy(&pre_tool.stderr)
    );
    assert!(pre_tool.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn codex_user_prompt_submit_ignores_stale_backlog_for_direct_eyes_reply() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let stale = Command::new(bin("eyes"))
        .args([
            "message",
            "send",
            "stale note",
            "--watcher",
            "debug",
            "--severity",
            "warning",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        stale.status.success(),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );

    let sender_socket_path = socket_path.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        enqueue(
            &sender_socket_path,
            "hook",
            serde_json::json!({
                "watcher": "debug",
                "severity": "info",
                "text": "@eyes: fresh pong",
                "target_harness": "codex",
                "target_session_id": "codex-direct-stale"
            }),
        );
    });

    let payload =
        r#"{"session_id":"codex-direct-stale","prompt":"@eyes say fresh pong","timestamp_ms":42}"#;
    let output = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sender.join().unwrap();

    let rendered = String::from_utf8(output.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!additional_context.contains("stale note"));
    assert!(additional_context.contains("- debug info: @eyes: fresh pong"));
    assert!(!additional_context.contains("<extra-eyes"));

    let pre_tool_payload = r#"{"session_id":"codex-direct-stale","tool_name":"Bash","tool_input":{"command":"pwd"},"timestamp_ms":43}"#;
    let pre_tool = run_codex_hook_cli(&project, &runtime, "PreToolUse", pre_tool_payload);
    assert!(
        pre_tool.status.success(),
        "{}",
        String::from_utf8_lossy(&pre_tool.stderr)
    );
    assert!(pre_tool.stdout.is_empty());

    stop_daemon(&project, &runtime);
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn codex_user_prompt_submit_waits_for_targeted_direct_reply_not_broadcast_noise() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    let sender_socket_path = socket_path.clone();
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        enqueue(
            &sender_socket_path,
            "hook",
            serde_json::json!({
                "watcher": "debug",
                "severity": "warning",
                "text": "fresh broadcast noise"
            }),
        );
        thread::sleep(Duration::from_millis(200));
        enqueue(
            &sender_socket_path,
            "hook",
            serde_json::json!({
                "watcher": "debug",
                "severity": "info",
                "text": "@eyes: targeted answer",
                "target_harness": "codex",
                "target_session_id": "codex-direct-targeted"
            }),
        );
    });

    let payload = r#"{"session_id":"codex-direct-targeted","prompt":"@eyes answer only me","timestamp_ms":42}"#;
    let output = run_codex_hook_cli(&project, &runtime, "UserPromptSubmit", payload);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sender.join().unwrap();

    let rendered = String::from_utf8(output.stdout).unwrap();
    let hook_output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        hook_output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let additional_context = hook_output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!additional_context.contains("fresh broadcast noise"));
    assert!(additional_context.contains("- debug info: @eyes: targeted answer"));
    assert!(!additional_context.contains("<extra-eyes"));

    let pre_tool_payload = r#"{"session_id":"codex-direct-targeted","tool_name":"Bash","tool_input":{"command":"pwd"},"timestamp_ms":43}"#;
    let pre_tool = run_codex_hook_cli(&project, &runtime, "PreToolUse", pre_tool_payload);
    assert!(
        pre_tool.status.success(),
        "{}",
        String::from_utf8_lossy(&pre_tool.stderr)
    );
    assert!(pre_tool.stdout.is_empty());

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

#[test]
fn hook_fetch_large_backlog_is_bounded_before_ipc_frame_limit() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let mut daemon = spawn_daemon(&project, &runtime);
    let status = wait_for_status(&project, &runtime, &mut daemon);
    let socket_path = socket_path_from_status(&status);

    for index in 0..4 {
        enqueue(
            &socket_path,
            "hook",
            serde_json::json!({
                "watcher": "huge",
                "severity": "warning",
                "text": format!("large-{index}:{}", "x".repeat(300 * 1024))
            }),
        );
    }

    let fetch = Command::new(bin("eyes"))
        .args([
            "hook",
            "fetch",
            "--cursor-key",
            "codex:large-backlog:hook",
            "--project",
        ])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    assert!(fetch.stdout.len() <= DEFAULT_HOOK_OUTPUT_BUDGET_BYTES + 1024);
    let rendered = String::from_utf8(fetch.stdout).unwrap();
    assert!(rendered.contains("large-0"), "{rendered}");

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
fn status_reports_empty_daemon_list_when_current_project_is_absent() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let runtime = temp.path().join("runtime");
    fs::create_dir_all(&project).unwrap();

    let output = Command::new(bin("eyes"))
        .args(["status", "--json", "--project"])
        .arg(&project)
        .env("EXTRA_EYES_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["status"], "not_running");
    assert_eq!(
        status["current_project_root"],
        project.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(status["current_pid"].is_null());
    assert!(status["daemons"].as_array().unwrap().is_empty());
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
harness = "raw"
model = "local-shell"
[settings]
command = ["sh", "-c", "cat >/dev/null"]
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
    assert_eq!(json["profile"]["harness"], "raw");
}

fn wait_for_status(
    project: &std::path::Path,
    runtime: &std::path::Path,
    daemon: &mut Child,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        if let Some(status) = daemon.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut stream) = daemon.stderr.take() {
                let _ = stream.read_to_string(&mut stderr);
            }
            panic!("daemon exited early with status {status}: {stderr}");
        }
        let output = Command::new(bin("eyes"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if output.status.success() {
            let status: Value = serde_json::from_slice(&output.stdout).unwrap();
            if status["status"] == "running" && status["pid"].as_u64().is_some() {
                return status;
            }
            last_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        }
        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become ready: stdout={last_stdout:?} stderr={last_stderr:?}");
}

fn wait_for_watch_status(
    project: &std::path::Path,
    runtime: &std::path::Path,
    process: &mut Child,
    expected_active: bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        if let Some(status) = process.try_wait().unwrap() {
            if expected_active {
                let mut stderr = String::new();
                if let Some(mut stream) = process.stderr.take() {
                    let _ = stream.read_to_string(&mut stderr);
                }
                panic!("watch process exited early with status {status}: {stderr}");
            }
        }
        let output = Command::new(bin("eyes"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if output.status.success() {
            let status: Value = serde_json::from_slice(&output.stdout).unwrap();
            if status["status"] == "running"
                && status["pid"].as_u64().is_some()
                && status["watch"]["active"] == expected_active
            {
                return status;
            }
            last_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        }
        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "watch status did not become active={expected_active}: stdout={last_stdout:?} stderr={last_stderr:?}"
    );
}

fn wait_for_child_exit(child: &mut Child, label: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let mut stderr = String::new();
    if let Some(mut stream) = child.stderr.take() {
        let _ = stream.read_to_string(&mut stderr);
    }
    panic!("{label} did not exit: {stderr}");
}

fn wait_for_bare_eyes_status(
    project: &std::path::Path,
    runtime: &std::path::Path,
    watch: &mut Child,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        if let Some(status) = watch.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut stream) = watch.stderr.take() {
                let _ = stream.read_to_string(&mut stderr);
            }
            panic!("bare eyes exited early with status {status}: {stderr}");
        }
        let output = Command::new(bin("eyes"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if output.status.success() {
            let status: Value = serde_json::from_slice(&output.stdout).unwrap();
            if status["status"] == "running" && status["pid"].as_u64().is_some() {
                return status;
            }
            last_stdout = String::from_utf8_lossy(&output.stdout).to_string();
        }
        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!("bare eyes did not auto-start daemon: stdout={last_stdout:?} stderr={last_stderr:?}");
}

fn spawn_daemon(project: &std::path::Path, runtime: &std::path::Path) -> Child {
    Command::new(bin("eyes"))
        .args(["daemon", "foreground", "--project"])
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
    let stop = Command::new(bin("eyes"))
        .args(["daemon", "stop", "--project"])
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
        let status = Command::new(bin("eyes"))
            .args(["status", "--json", "--project"])
            .arg(project)
            .env("EXTRA_EYES_RUNTIME_DIR", runtime)
            .output()
            .unwrap();
        if status.status.success() {
            let parsed: Value = serde_json::from_slice(&status.stdout).unwrap();
            if parsed["status"] == "not_running" {
                return;
            }
        } else if String::from_utf8_lossy(&status.stderr).contains("no eyes daemon is running") {
            return;
        }
        last_stdout = String::from_utf8_lossy(&status.stdout).to_string();
        last_stderr = String::from_utf8_lossy(&status.stderr).to_string();
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not stop; stdout={last_stdout:?} stderr={last_stderr:?}");
}

fn wait_for_log_line_count(path: &std::path::Path, expected: usize, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut latest = String::new();
    while Instant::now() < deadline {
        latest = fs::read_to_string(path).unwrap_or_default();
        if latest.lines().count() >= expected {
            return latest;
        }
        thread::sleep(Duration::from_millis(25));
    }
    latest
}

fn prepend_path(path: &std::path::Path) -> std::ffi::OsString {
    let mut paths = vec![path.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap()
}

fn wait_for_path(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("path did not appear: {}", path.display());
}

fn wait_for_watch_scoped_check_in(project: &std::path::Path) {
    let path = project.join(".eyes/state/messages.jsonl");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_contents = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(&path) {
            if contents.contains("\"kind\":\"watcher_check_in\"")
                && contents.contains("\"target_session_id\":\"watch:")
            {
                return;
            }
            last_contents = contents;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "watch-scoped check-in did not appear in {}: {last_contents}",
        path.display()
    );
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

fn run_codex_hook_cli_from_cwd_without_project(
    cwd: &std::path::Path,
    runtime: &std::path::Path,
    event: &str,
    payload: &str,
) -> std::process::Output {
    let mut child = Command::new(bin("eyes"))
        .args(["hook", "codex", "--event", event])
        .current_dir(cwd)
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

fn run_claude_code_hook_cli_from_cwd_without_project(
    cwd: &std::path::Path,
    runtime: &std::path::Path,
    event: &str,
    payload: &str,
) -> std::process::Output {
    let mut child = Command::new(bin("eyes"))
        .args(["hook", "claude-code", "--event", event])
        .current_dir(cwd)
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
            after_message_id: None,
            targeted_only: false,
            include_all_targets: false,
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
    run_watcher_for_route(socket_path, profile, tick_id, context, None, None, None)
}

fn run_watcher_for_route(
    socket_path: &std::path::Path,
    profile: &str,
    tick_id: &str,
    context: WatcherContext,
    target_harness: Option<&str>,
    target_session_id: Option<&str>,
    source_event_id: Option<u64>,
) -> Response {
    send_request(
        socket_path,
        &Request::RunWatcher {
            protocol: PROTOCOL_VERSION,
            profile: Some(profile.to_owned()),
            tick_id: tick_id.to_owned(),
            context,
            target_harness: target_harness.map(str::to_owned),
            target_session_id: target_session_id.map(str::to_owned),
            source_event_id,
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
reasoning_effort = "low"
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
