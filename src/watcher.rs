use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::context::{budget_context, ContextBudgetReport};
use crate::conversation::ConversationEvent;
use crate::profiles::{integer_setting, Harness, WatcherProfile};
use crate::{EyesError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct WatcherContext {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub conversation: Vec<ConversationEvent>,
    #[serde(default)]
    pub budget: ContextBudgetReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatcherInputEnvelope {
    pub v: u32,
    pub watcher: String,
    pub tick_id: String,
    pub prompt: String,
    pub context: WatcherContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub units: u64,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatcherMessage {
    pub watcher: String,
    pub tick_id: String,
    pub severity: String,
    pub refs: Vec<WatcherRef>,
    pub text: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatcherRef {
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub column: Option<u64>,
    #[serde(default)]
    pub end_line: Option<u64>,
    #[serde(default)]
    pub end_column: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatcherStatusEvent {
    pub watcher: String,
    pub tick_id: String,
    pub severity: String,
    pub outcome: String,
    pub text: String,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatcherRunResult {
    pub watcher: String,
    pub tick_id: String,
    pub messages: Vec<WatcherMessage>,
    pub statuses: Vec<WatcherStatusEvent>,
}

#[derive(Debug, Clone)]
struct RawWatcherSettings {
    command: Vec<String>,
    timeout_ms: u64,
    cost_limit_units: Option<u64>,
    context_budget_bytes: Option<u64>,
    env: Vec<(String, String)>,
}

#[derive(Debug)]
struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    elapsed_ms: u64,
    timed_out: bool,
}

#[derive(Debug, Deserialize)]
struct StdoutEvent {
    v: u32,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    text: Option<String>,
    #[serde(default)]
    refs: Vec<WatcherRef>,
    #[serde(default)]
    usage: Option<Usage>,
}

pub fn run_profile(
    profile: &WatcherProfile,
    project_root: &Path,
    tick_id: impl Into<String>,
    context: WatcherContext,
) -> Result<WatcherRunResult> {
    let tick_id = tick_id.into();
    let settings = match profile.harness {
        Harness::Raw => RawWatcherSettings::from_profile(profile)?,
        _ => {
            if profile.settings.contains_key("command") {
                return Err(EyesError::Config(
                    "settings.command is only supported for harness='raw'".to_owned(),
                ));
            }
            return Err(EyesError::Config(format!(
                "watcher runner harness '{}' is unsupported; use harness='raw' for watcher profiles",
                profile.harness.as_str()
            )));
        }
    };

    if matches!(settings.cost_limit_units, Some(0)) {
        return Ok(WatcherRunResult {
            watcher: profile.name.clone(),
            tick_id: tick_id.clone(),
            messages: Vec::new(),
            statuses: vec![status(
                profile,
                &tick_id,
                "warning",
                "cost_limit_exceeded",
                "Watcher skipped because cost_limit_units is 0.",
                json!({"cost_limit_units": 0}),
            )],
        });
    }

    let context = match settings.context_budget_bytes {
        Some(max_bytes) => budget_context(context, max_bytes as usize),
        None => context,
    };

    let input = WatcherInputEnvelope {
        v: 1,
        watcher: profile.name.clone(),
        tick_id: tick_id.clone(),
        prompt: profile.prompt.clone(),
        context,
    };
    let stdin = serde_json::to_vec(&input)?;
    let output = run_process(&settings, project_root, &profile.name, &tick_id, &stdin)?;
    let mut result = parse_stdout(profile, &tick_id, &output, settings.cost_limit_units);

    if output.timed_out {
        result.statuses.push(status(
            profile,
            &tick_id,
            "error",
            "timeout",
            &format!("Watcher timed out after {} ms.", settings.timeout_ms),
            json!({"timeout_ms": settings.timeout_ms, "elapsed_ms": output.elapsed_ms}),
        ));
    } else if output.exit_code.unwrap_or(0) != 0 {
        result.statuses.push(status(
            profile,
            &tick_id,
            "error",
            "nonzero_exit",
            &format!(
                "Watcher exited with status {}.",
                output.exit_code.unwrap_or(-1)
            ),
            json!({
                "exit_code": output.exit_code,
                "elapsed_ms": output.elapsed_ms,
                "stderr_excerpt": excerpt(&String::from_utf8_lossy(&output.stderr), 512),
            }),
        ));
    }

    Ok(result)
}

impl RawWatcherSettings {
    fn from_profile(profile: &WatcherProfile) -> Result<Self> {
        let command = string_array_setting(profile, "command")?.ok_or_else(|| {
            EyesError::Config(format!(
                "raw watcher profile '{}' requires settings.command",
                profile.name
            ))
        })?;
        if command.is_empty() {
            return Err(EyesError::Config(format!(
                "raw watcher profile '{}' has an empty settings.command",
                profile.name
            )));
        }
        let timeout_ms = integer_setting(profile, "timeout_ms")?.unwrap_or(10_000);
        if timeout_ms == 0 {
            return Err(EyesError::Config(format!(
                "raw watcher profile '{}' timeout_ms must be greater than zero",
                profile.name
            )));
        }
        let cost_limit_units = integer_setting(profile, "cost_limit_units")?;
        let context_budget_bytes = integer_setting(profile, "context_budget_bytes")?;
        if matches!(context_budget_bytes, Some(bytes) if bytes < 1024) {
            return Err(EyesError::Config(format!(
                "raw watcher profile '{}' context_budget_bytes must be at least 1024",
                profile.name
            )));
        }
        let env = env_setting(profile)?;

        Ok(Self {
            command,
            timeout_ms,
            cost_limit_units,
            context_budget_bytes,
            env,
        })
    }
}

fn string_array_setting(profile: &WatcherProfile, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = profile.settings.get(key) else {
        return Ok(None);
    };
    let Some(array) = value.as_array() else {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.{key} must be an array of strings",
            profile.name
        )));
    };
    let mut out = Vec::new();
    for item in array {
        let Some(text) = item.as_str() else {
            return Err(EyesError::Config(format!(
                "profile '{}' settings.{key} must be an array of strings",
                profile.name
            )));
        };
        out.push(text.to_owned());
    }
    Ok(Some(out))
}

fn env_setting(profile: &WatcherProfile) -> Result<Vec<(String, String)>> {
    let Some(value) = profile.settings.get("env") else {
        return Ok(Vec::new());
    };
    let Some(table) = value.as_table() else {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.env must be a table",
            profile.name
        )));
    };
    let mut out = Vec::new();
    for (key, value) in table {
        let Some(text) = value.as_str() else {
            return Err(EyesError::Config(format!(
                "profile '{}' settings.env values must be strings",
                profile.name
            )));
        };
        out.push((key.clone(), text.to_owned()));
    }
    Ok(out)
}

fn run_process(
    settings: &RawWatcherSettings,
    project_root: &Path,
    watcher: &str,
    tick_id: &str,
    stdin: &[u8],
) -> Result<ProcessOutput> {
    let started = Instant::now();
    let mut command = Command::new(&settings.command[0]);
    command
        .args(&settings.command[1..])
        .current_dir(project_root)
        .env("EXTRA_EYES_WATCHER_NAME", watcher)
        .env("EXTRA_EYES_TICK_ID", tick_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    for (key, value) in &settings.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        EyesError::Config(format!(
            "failed to spawn watcher command '{}': {error}",
            settings.command[0]
        ))
    })?;

    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin.write_all(stdin)?;
    }

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let stdout_handle = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stderr_handle = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });

    let timeout = Duration::from_millis(settings.timeout_ms);
    let (exit_code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status.code(), false);
        }
        if started.elapsed() >= timeout {
            let _ = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    Ok(ProcessOutput {
        stdout,
        stderr,
        exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out,
    })
}

fn parse_stdout(
    profile: &WatcherProfile,
    tick_id: &str,
    output: &ProcessOutput,
    cost_limit_units: Option<u64>,
) -> WatcherRunResult {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut messages = Vec::new();
    let mut statuses = Vec::new();
    let mut usage_units = 0_u64;

    for (index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<StdoutEvent>(line) {
            Ok(event) => event,
            Err(error) => {
                statuses.push(status(
                    profile,
                    tick_id,
                    "warning",
                    "malformed_stdout",
                    "Watcher emitted malformed JSONL on stdout.",
                    json!({
                        "line_no": index + 1,
                        "raw_excerpt": excerpt(line, 512),
                        "error": error.to_string(),
                    }),
                ));
                continue;
            }
        };

        if event.v != 1 {
            statuses.push(status(
                profile,
                tick_id,
                "warning",
                "unsupported_stdout_event",
                "Watcher emitted an unsupported stdout event version.",
                json!({"line_no": index + 1, "v": event.v}),
            ));
            continue;
        }
        if event.event_type == "status" {
            let severity = event.severity.unwrap_or_else(|| "info".to_owned());
            if !is_supported_severity(&severity) {
                statuses.push(status(
                    profile,
                    tick_id,
                    "warning",
                    "malformed_stdout",
                    "Watcher status has unsupported severity.",
                    json!({"line_no": index + 1, "severity": severity}),
                ));
                continue;
            }
            let Some(outcome) = event.outcome.filter(|value| !value.trim().is_empty()) else {
                statuses.push(status(
                    profile,
                    tick_id,
                    "warning",
                    "malformed_stdout",
                    "Watcher status is missing non-empty outcome.",
                    json!({"line_no": index + 1}),
                ));
                continue;
            };
            let text = event.text.unwrap_or_else(|| outcome.clone());
            statuses.push(status(
                profile,
                tick_id,
                &severity,
                &outcome,
                &text,
                json!({"line_no": index + 1}),
            ));
            continue;
        }

        if event.event_type != "message" {
            statuses.push(status(
                profile,
                tick_id,
                "warning",
                "unsupported_stdout_event",
                "Watcher emitted an unsupported stdout event type.",
                json!({"line_no": index + 1, "type": event.event_type}),
            ));
            continue;
        }

        let text = match event.text {
            Some(text) if !text.trim().is_empty() => text,
            _ => {
                statuses.push(status(
                    profile,
                    tick_id,
                    "warning",
                    "malformed_stdout",
                    "Watcher message is missing non-empty text.",
                    json!({"line_no": index + 1}),
                ));
                continue;
            }
        };
        let severity = event.severity.unwrap_or_else(|| "info".to_owned());
        if !is_supported_severity(&severity) {
            statuses.push(status(
                profile,
                tick_id,
                "warning",
                "malformed_stdout",
                "Watcher message has unsupported severity.",
                json!({"line_no": index + 1, "severity": severity}),
            ));
            continue;
        }

        if let Some(usage) = &event.usage {
            usage_units = usage_units.saturating_add(usage.units);
        }
        messages.push(WatcherMessage {
            watcher: profile.name.clone(),
            tick_id: tick_id.to_owned(),
            severity,
            refs: event.refs,
            text,
            usage: event.usage,
        });

        if let Some(limit) = cost_limit_units {
            if usage_units > limit {
                statuses.push(status(
                    profile,
                    tick_id,
                    "warning",
                    "cost_limit_exceeded",
                    "Watcher exceeded cost_limit_units.",
                    json!({"cost_limit_units": limit, "reported_units": usage_units}),
                ));
                break;
            }
        }
    }

    WatcherRunResult {
        watcher: profile.name.clone(),
        tick_id: tick_id.to_owned(),
        messages,
        statuses,
    }
}

fn is_supported_severity(severity: &str) -> bool {
    matches!(severity, "info" | "warning" | "error")
}

fn status(
    profile: &WatcherProfile,
    tick_id: &str,
    severity: &str,
    outcome: &str,
    text: &str,
    details: Value,
) -> WatcherStatusEvent {
    WatcherStatusEvent {
        watcher: profile.name.clone(),
        tick_id: tick_id.to_owned(),
        severity: severity.to_owned(),
        outcome: outcome.to_owned(),
        text: text.to_owned(),
        details,
    }
}

fn excerpt(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
