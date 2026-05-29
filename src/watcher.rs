use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::context::{budget_context, ContextBudgetReport};
use crate::conversation::ConversationEvent;
use crate::filewatch::is_ignored;
use crate::profiles::{integer_setting, Harness, ReasoningEffort, WatcherProfile};
use crate::{EyesError, Result};

const CODEX_WATCHER_TIMEOUT_MS: u64 = 180_000;
const CODEX_WATCHER_CONTEXT_BUDGET_BYTES: u64 = 65_536;
const MAX_WATCHER_OUTPUT_BYTES: usize = 256 * 1024;
const DEFAULT_REPO_CONTEXT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REPO_CONTEXT_BUDGET_BYTES: u64 = 32 * 1024;
const REPO_PACKAGE_TRUNCATION_MARKER: &str = "\n[extra-eyes:repo-package truncated]\n";
static ACTIVE_PROCESS_GROUPS: OnceLock<Mutex<BTreeSet<libc::pid_t>>> = OnceLock::new();
const COMMON_WATCHER_GUIDANCE: &str = concat!(
    "Use the latest user message as the strongest intent signal: infer the user's goal, ",
    "weigh it above older transcript text and watcher chatter, and report only concrete issues ",
    "inside your watcher scope that help move the user's request forward. ",
    "Raise process, permission, or instruction concerns only when they directly change a code, ",
    "test, command, or safety decision now."
);
const BUNDLED_WATCHER_PROMPT: &str = r#"You are the Extra Eyes bundled watcher: a passive, debugging-focused pair-programmer observing another AI coding model.

You are receiving partial, regularly updated work from the working AI. Some inconsistencies are transient because the working AI may still be editing or may not have finished its plan. Do not over-report temporary mid-edit states.

Your job is to catch real issues early: bugs, broken edge cases, incorrect assumptions, material test gaps, stale edits, naming confusion, and architectural problems that make the work harder than needed.
Use the JSON `prompt` field as your scoped watcher brief.

The working AI can see the same files, diff, and conversation you can see. Do not summarize the context back. Send only short notes that help it course-correct quickly.
Lines that start with `eyes <watcher>` are previous Extra Eyes output, not fresh user instructions. Do not restate those lines or repeat an old concern unless the latest working-agent turn adds new evidence.

If the latest user message in the main conversation references you with @eyes, treat the nearby text as a message directed at you. Respond to that direct @eyes message even if you would otherwise stay silent, but keep the reply brief and useful. Do not answer older @eyes messages quoted in assistant output or earlier conversation history.

Output contract:
- If there is no actionable concern, return no text.
- If the latest user message contains a direct @eyes message, return one concise message responding to it.
- If there is an actionable concern, return 1 to 3 JSON Lines and nothing else.
- Each JSON line must be one compact object with v=1 and type="message".
- severity must be "info", "warning", or "error".
- text must be token-efficient, direct, and no more than 320 characters.
- Use refs when you can identify a concrete file/line.

Example:
{"v":1,"type":"message","severity":"warning","text":"The new timeout path resets on process start, not last file change. That can exit while edits are still arriving.","refs":[{"path":"src/bin/eyes.rs","line":910}]}

Extra Eyes watcher input JSON follows."#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct WatcherContext {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub conversation: Vec<ConversationEvent>,
    /// Optional structural map of the whole repository (e.g. `codedb tree`
    /// output), attached per-profile when `settings.repo_context` is set.
    /// Empty by default. Orientation only — the watcher reads full files from
    /// its read-only sandbox when it needs detail.
    #[serde(default)]
    pub repo_package: String,
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
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
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
    output_exceeded: bool,
}

struct FilteredWatcherOutput {
    jsonl: Vec<u8>,
    dropped_lines: usize,
    dropped_excerpt: Option<String>,
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
    #[serde(default)]
    details: Option<Value>,
}

pub fn run_profile(
    profile: &WatcherProfile,
    project_root: &Path,
    tick_id: impl Into<String>,
    mut context: WatcherContext,
) -> Result<WatcherRunResult> {
    let tick_id = tick_id.into();
    let settings = match profile.harness {
        Harness::Raw => RawWatcherSettings::from_profile(profile)?,
        Harness::Codex => return run_codex_profile(profile, project_root, &tick_id, context),
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

    let repo_status = attach_repo_package(profile, project_root, &tick_id, &mut context)?;
    let context = match settings.context_budget_bytes {
        Some(max_bytes) => budget_context(context, max_bytes as usize),
        None => context,
    };

    let input = WatcherInputEnvelope {
        v: 1,
        watcher: profile.name.clone(),
        tick_id: tick_id.clone(),
        prompt: effective_watcher_prompt(&profile.prompt),
        context,
    };
    let stdin = serde_json::to_vec(&input)?;
    let output = run_process(&settings, project_root, &profile.name, &tick_id, &stdin)?;
    let mut result = parse_stdout(profile, &tick_id, &output, settings.cost_limit_units);
    if let Some(repo_status) = repo_status {
        result.statuses.push(repo_status);
    }

    if output.timed_out {
        result.statuses.push(status(
            profile,
            &tick_id,
            "error",
            "timeout",
            &format!("timed out after {}", human_duration_ms(settings.timeout_ms)),
            json!({"timeout_ms": settings.timeout_ms, "elapsed_ms": output.elapsed_ms}),
        ));
    } else if output.output_exceeded {
        result.statuses.push(status(
            profile,
            &tick_id,
            "error",
            "output_limit_exceeded",
            "Watcher output exceeded the per-tick capture limit.",
            json!({"max_output_bytes": MAX_WATCHER_OUTPUT_BYTES, "elapsed_ms": output.elapsed_ms}),
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

pub fn kill_active_process_groups() {
    let groups = active_process_groups();
    let process_groups = groups
        .lock()
        .map(|groups| groups.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for process_group in process_groups {
        let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
    }
}

fn effective_watcher_prompt(profile_prompt: &str) -> String {
    format!(
        "{COMMON_WATCHER_GUIDANCE}\n\nWatcher scope:\n{}",
        profile_prompt.trim()
    )
}

/// Reads the `settings.repo_context` profile knob. Currently only `"codedb"`
/// is supported; any other non-empty value is a config error.
fn repo_context_mode(profile: &WatcherProfile) -> Result<Option<String>> {
    let Some(value) = profile.settings.get("repo_context") else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.repo_context must be a string",
            profile.name
        )));
    };
    let trimmed = text.trim();
    match trimmed {
        "" | "off" | "none" => Ok(None),
        "codedb" => Ok(Some(trimmed.to_owned())),
        other => Err(EyesError::Config(format!(
            "profile '{}' settings.repo_context '{other}' is unsupported; use \"codedb\"",
            profile.name
        ))),
    }
}

/// Best-effort: when `settings.repo_context` is enabled, build the repo package
/// and attach it to `context.repo_package`. On failure returns a diagnostic
/// status the caller appends to the run result (surfaced once, then suppressed
/// by the daemon's failure dedup). Returns `Ok(None)` when the knob is off.
fn attach_repo_package(
    profile: &WatcherProfile,
    project_root: &Path,
    tick_id: &str,
    context: &mut WatcherContext,
) -> Result<Option<WatcherStatusEvent>> {
    let Some(mode) = repo_context_mode(profile)? else {
        return Ok(None);
    };
    debug_assert_eq!(mode, "codedb");
    let timeout_ms = integer_setting(profile, "repo_context_timeout_ms")?
        .unwrap_or(DEFAULT_REPO_CONTEXT_TIMEOUT_MS);
    let budget_bytes = integer_setting(profile, "repo_context_budget_bytes")?
        .unwrap_or(DEFAULT_REPO_CONTEXT_BUDGET_BYTES) as usize;

    match fetch_codedb_tree(project_root, timeout_ms, budget_bytes) {
        Ok(package) if !package.trim().is_empty() => {
            context.repo_package = package;
            Ok(None)
        }
        Ok(_) => Ok(Some(status(
            profile,
            tick_id,
            "warning",
            "repo_context_failed",
            "codedb tree returned no usable repo map.",
            json!({"source": "codedb"}),
        ))),
        Err(reason) => Ok(Some(status(
            profile,
            tick_id,
            "warning",
            "repo_context_failed",
            &reason,
            json!({"source": "codedb", "timeout_ms": timeout_ms}),
        ))),
    }
}

fn codedb_binary() -> String {
    std::env::var("CODEDB_BINARY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "codedb".to_owned())
}

/// Runs `codedb tree` in the project root, filters out `eyes`-ignored paths,
/// and caps the result to `budget_bytes`. Returns `Err(reason)` on any failure
/// (binary missing, nonzero exit, timeout) so the caller can surface it once.
fn fetch_codedb_tree(
    project_root: &Path,
    timeout_ms: u64,
    budget_bytes: usize,
) -> std::result::Result<String, String> {
    let started = Instant::now();
    let mut command = Command::new(codedb_binary());
    command
        .arg("tree")
        .current_dir(project_root)
        // codedb honours CODEDB_NO_TELEMETRY; keep watcher ticks fully local.
        .env("CODEDB_NO_TELEMETRY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);

    let output = run_command_with_deadline(command, &[], timeout_ms, started)
        .map_err(|error| format!("could not run codedb tree: {error}"))?;
    if output.timed_out {
        return Err(format!(
            "codedb tree timed out after {}",
            human_duration_ms(timeout_ms)
        ));
    }
    if output.exit_code.unwrap_or(0) != 0 {
        return Err(format!(
            "codedb tree exited with status {}: {}",
            output.exit_code.unwrap_or(-1),
            excerpt(&String::from_utf8_lossy(&output.stderr), 256)
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(cap_repo_package(&filter_codedb_tree(&raw), budget_bytes))
}

/// Drops the `✓ indexed` status banner and any tree blocks rooted at a path
/// `eyes` already ignores (`.git`, `.eyes`, `node_modules`, `target`, lockfiles,
/// …). codedb tree is indentation-nested, so an ignored directory header skips
/// every deeper-indented descendant until indentation returns to its level.
fn filter_codedb_tree(raw: &str) -> String {
    let mut out = String::new();
    let mut skip_below: Option<usize> = None;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // codedb prints a "✓ indexed  15.0ms" banner first; drop status lines.
        if line.trim_start().starts_with('\u{2713}') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(threshold) = skip_below {
            if indent > threshold {
                continue;
            }
            skip_below = None;
        }
        let token = line.trim_start().split_whitespace().next().unwrap_or("");
        let name = token.trim_end_matches('/');
        if !name.is_empty() && is_ignored(Path::new(name)) {
            if token.ends_with('/') {
                skip_below = Some(indent);
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn cap_repo_package(package: &str, budget_bytes: usize) -> String {
    if package.len() <= budget_bytes {
        return package.to_owned();
    }
    let keep = budget_bytes.saturating_sub(REPO_PACKAGE_TRUNCATION_MARKER.len());
    let mut end = keep.min(package.len());
    while end > 0 && !package.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{REPO_PACKAGE_TRUNCATION_MARKER}", &package[..end])
}

fn run_codex_profile(
    profile: &WatcherProfile,
    project_root: &Path,
    tick_id: &str,
    mut context: WatcherContext,
) -> Result<WatcherRunResult> {
    let timeout_ms = integer_setting(profile, "timeout_ms")?.unwrap_or(CODEX_WATCHER_TIMEOUT_MS);
    let budget_bytes = integer_setting(profile, "context_budget_bytes")?
        .unwrap_or(CODEX_WATCHER_CONTEXT_BUDGET_BYTES);
    let reasoning_effort = profile.reasoning_effort.ok_or_else(|| {
        EyesError::Config(format!(
            "codex watcher profile '{}' requires top-level reasoning_effort",
            profile.name
        ))
    })?;
    let repo_status = attach_repo_package(profile, project_root, tick_id, &mut context)?;
    let context = budget_context(context, budget_bytes as usize);
    let input = WatcherInputEnvelope {
        v: 1,
        watcher: profile.name.clone(),
        tick_id: tick_id.to_owned(),
        prompt: effective_watcher_prompt(&profile.prompt),
        context,
    };
    let input_json = serde_json::to_string(&input)?;
    let output_path = bundled_output_path(&profile.name, tick_id);
    let stdin = format!(
        "{BUNDLED_WATCHER_PROMPT}\n\n<watcher-input-json>\n{input_json}\n</watcher-input-json>\n"
    );

    let output = match run_codex_process(
        project_root,
        &output_path,
        &profile.model,
        reasoning_effort,
        timeout_ms,
        stdin.as_bytes(),
    ) {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&output_path);
            return Ok(WatcherRunResult {
                watcher: profile.name.clone(),
                tick_id: tick_id.to_owned(),
                messages: Vec::new(),
                statuses: vec![status(
                    profile,
                    tick_id,
                    "warning",
                    "codex_cli_failed",
                    "Codex watcher could not start Codex CLI.",
                    json!({"error": error.to_string()}),
                )],
            });
        }
    };

    let model_output = fs::read(&output_path).unwrap_or_default();
    let _ = fs::remove_file(&output_path);
    let filtered_output = filter_watcher_jsonl(&model_output);
    let parse_output = ProcessOutput {
        stdout: filtered_output.jsonl,
        stderr: output.stderr,
        exit_code: output.exit_code,
        elapsed_ms: output.elapsed_ms,
        timed_out: output.timed_out,
        output_exceeded: output.output_exceeded,
    };
    let mut result = parse_stdout(profile, tick_id, &parse_output, None);
    if let Some(repo_status) = repo_status {
        result.statuses.push(repo_status);
    }
    if filtered_output.dropped_lines > 0 {
        result.statuses.push(status(
            profile,
            tick_id,
            "warning",
            "malformed_stdout",
            "Codex watcher emitted non-JSONL model output.",
            json!({
                "dropped_line_count": filtered_output.dropped_lines,
                "raw_excerpt": filtered_output.dropped_excerpt,
            }),
        ));
    }

    if parse_output.timed_out {
        result.statuses.push(status(
            profile,
            tick_id,
            "error",
            "timeout",
            &format!("timed out after {}", human_duration_ms(timeout_ms)),
            json!({"timeout_ms": timeout_ms, "elapsed_ms": parse_output.elapsed_ms}),
        ));
    } else if parse_output.output_exceeded {
        result.statuses.push(status(
            profile,
            tick_id,
            "error",
            "output_limit_exceeded",
            "Watcher output exceeded the per-tick capture limit.",
            json!({"max_output_bytes": MAX_WATCHER_OUTPUT_BYTES, "elapsed_ms": parse_output.elapsed_ms}),
        ));
    } else if parse_output.exit_code.unwrap_or(0) != 0 {
        result.statuses.push(status(
            profile,
            tick_id,
            "warning",
            "codex_cli_failed",
            "Codex watcher could not query Codex CLI.",
            json!({
                "exit_code": parse_output.exit_code,
                "elapsed_ms": parse_output.elapsed_ms,
                "stderr_excerpt": excerpt(&String::from_utf8_lossy(&parse_output.stderr), 512),
            }),
        ));
    }

    Ok(result)
}

fn run_codex_process(
    project_root: &Path,
    output_path: &Path,
    model: &str,
    reasoning_effort: ReasoningEffort,
    timeout_ms: u64,
    stdin: &[u8],
) -> Result<ProcessOutput> {
    let started = Instant::now();
    let mut command = Command::new("codex");
    command
        .arg("exec")
        .arg("--ignore-user-config")
        .arg("--model")
        .arg(model)
        .arg("-c")
        .arg(format!(
            "model_reasoning_effort=\"{}\"",
            reasoning_effort.as_str()
        ))
        .arg("--sandbox")
        .arg("read-only")
        .arg("--ephemeral")
        .arg("--skip-git-repo-check")
        .arg("--output-last-message")
        .arg(output_path)
        .arg("-")
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command.process_group(0);
    run_command_with_deadline(command, stdin, timeout_ms, started)
}

fn bundled_output_path(watcher: &str, tick_id: &str) -> PathBuf {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let name = format!(
        "extra-eyes-{}-{}-{}-{now_ns}.jsonl",
        sanitize_path_part(watcher),
        sanitize_path_part(tick_id),
        std::process::id()
    );
    std::env::temp_dir().join(name)
}

fn sanitize_path_part(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn filter_watcher_jsonl(bytes: &[u8]) -> FilteredWatcherOutput {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut dropped_lines = 0_usize;
    let mut dropped_excerpt = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if serde_json::from_str::<StdoutEvent>(trimmed)
            .map(|event| event.v == 1 && matches!(event.event_type.as_str(), "message" | "status"))
            .unwrap_or(false)
        {
            out.push_str(trimmed);
            out.push('\n');
        } else {
            dropped_lines += 1;
            if dropped_excerpt.is_none() {
                dropped_excerpt = Some(excerpt(trimmed, 512));
            }
        }
    }
    FilteredWatcherOutput {
        jsonl: out.into_bytes(),
        dropped_lines,
        dropped_excerpt,
    }
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
            model: profile.model.clone(),
            reasoning_effort: profile.reasoning_effort,
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
        .env("EXTRA_EYES_MODEL", &settings.model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(reasoning_effort) = settings.reasoning_effort {
        command.env("EXTRA_EYES_REASONING_EFFORT", reasoning_effort.as_str());
    }
    command.process_group(0);
    for (key, value) in &settings.env {
        command.env(key, value);
    }
    run_command_with_deadline(command, stdin, settings.timeout_ms, started).map_err(|error| {
        if matches!(error, EyesError::Config(_)) {
            error
        } else {
            EyesError::Config(format!(
                "failed to run watcher command '{}': {error}",
                settings.command[0]
            ))
        }
    })
}

fn run_command_with_deadline(
    mut command: Command,
    stdin: &[u8],
    timeout_ms: u64,
    started: Instant,
) -> Result<ProcessOutput> {
    let mut child = command.spawn()?;
    let process_group = child.id() as libc::pid_t;
    register_process_group(process_group);
    let output_exceeded = Arc::new(AtomicBool::new(false));

    let stdin_done = child.stdin.take().map(|mut child_stdin| {
        let (tx, rx) = mpsc::channel();
        let bytes = stdin.to_vec();
        let _ = thread::spawn(move || {
            let _ = child_stdin.write_all(&bytes);
            let _ = tx.send(());
        });
        rx
    });
    let stdout_rx = child
        .stdout
        .take()
        .map(|stdout| spawn_bounded_reader(stdout, Arc::clone(&output_exceeded)));
    let stderr_rx = child
        .stderr
        .take()
        .map(|stderr| spawn_bounded_reader(stderr, Arc::clone(&output_exceeded)));

    let timeout = Duration::from_millis(timeout_ms);
    let (exit_code, timed_out, capped_output) = loop {
        if output_exceeded.load(Ordering::SeqCst) {
            kill_process_group(process_group, &mut child);
            break (None, false, true);
        }
        if let Some(status) = child.try_wait()? {
            kill_process_group(process_group, &mut child);
            break (status.code(), false, false);
        }
        if started.elapsed() >= timeout {
            kill_process_group(process_group, &mut child);
            break (None, true, false);
        }
        thread::sleep(Duration::from_millis(10));
    };

    let _ = child.wait();
    unregister_process_group(process_group);
    let io_join_timeout = Duration::from_millis(250);
    if let Some(done) = stdin_done {
        let _ = done.recv_timeout(io_join_timeout);
    }
    let stdout = stdout_rx
        .map(|rx| rx.recv_timeout(io_join_timeout).unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_rx
        .map(|rx| rx.recv_timeout(io_join_timeout).unwrap_or_default())
        .unwrap_or_default();
    Ok(ProcessOutput {
        stdout,
        stderr,
        exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        timed_out,
        output_exceeded: capped_output || output_exceeded.load(Ordering::SeqCst),
    })
}

fn active_process_groups() -> &'static Mutex<BTreeSet<libc::pid_t>> {
    ACTIVE_PROCESS_GROUPS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn register_process_group(process_group: libc::pid_t) {
    if let Ok(mut groups) = active_process_groups().lock() {
        groups.insert(process_group);
    }
}

fn unregister_process_group(process_group: libc::pid_t) {
    if let Ok(mut groups) = active_process_groups().lock() {
        groups.remove(&process_group);
    }
}

fn spawn_bounded_reader<R>(mut reader: R, exceeded: Arc<AtomicBool>) -> mpsc::Receiver<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let _ = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            let remaining = MAX_WATCHER_OUTPUT_BYTES.saturating_sub(bytes.len());
            if remaining > 0 {
                bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            if read > remaining {
                exceeded.store(true, Ordering::SeqCst);
                break;
            }
        }
        let _ = tx.send(bytes);
    });
    rx
}

fn kill_process_group(process_group: libc::pid_t, child: &mut std::process::Child) {
    let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
    let _ = child.kill();
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
            let details = status_event_details(event.details, index + 1);
            statuses.push(status(
                profile, tick_id, &severity, &outcome, &text, details,
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

fn status_event_details(details: Option<Value>, line_no: usize) -> Value {
    match details {
        Some(Value::Object(mut map)) => {
            map.entry("line_no".to_owned())
                .or_insert_with(|| json!(line_no));
            Value::Object(map)
        }
        Some(value) => json!({"line_no": line_no, "details": value}),
        None => json!({"line_no": line_no}),
    }
}

fn excerpt(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn human_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if ms.is_multiple_of(1_000) {
        return format!("{}s", ms / 1_000);
    }
    let whole = ms / 1_000;
    let tenths = (ms % 1_000) / 100;
    if tenths == 0 {
        format!("{whole}s")
    } else {
        format!("{whole}.{tenths}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{Harness, ProfileSource, WatcherProfile};
    use std::collections::BTreeMap;

    fn profile_with_settings(settings: &[(&str, toml::Value)]) -> WatcherProfile {
        let _ = ProfileSource::Bundled;
        WatcherProfile {
            name: "t".to_owned(),
            default: false,
            prompt: "watch".to_owned(),
            harness: Harness::Codex,
            model: "m".to_owned(),
            reasoning_effort: None,
            settings: settings
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn repo_context_mode_parses_supported_and_off_values() {
        assert_eq!(
            repo_context_mode(&profile_with_settings(&[])).unwrap(),
            None
        );
        assert_eq!(
            repo_context_mode(&profile_with_settings(&[(
                "repo_context",
                toml::Value::String("codedb".to_owned())
            )]))
            .unwrap(),
            Some("codedb".to_owned())
        );
        for off in ["", "off", "none"] {
            assert_eq!(
                repo_context_mode(&profile_with_settings(&[(
                    "repo_context",
                    toml::Value::String(off.to_owned())
                )]))
                .unwrap(),
                None
            );
        }
        assert!(repo_context_mode(&profile_with_settings(&[(
            "repo_context",
            toml::Value::String("repomix".to_owned())
        )]))
        .is_err());
        assert!(repo_context_mode(&profile_with_settings(&[(
            "repo_context",
            toml::Value::Integer(1)
        )]))
        .is_err());
    }

    #[test]
    fn filter_codedb_tree_drops_banner_and_ignored_blocks() {
        let raw = "\u{2713} indexed  15.0ms\n\
            .eyes/\n  state/\n    conversation.jsonl  unknown  268L  0 sym\n  inbox.md  markdown  392L  0 sym\n\
            target/\n  debug/\n    eyes  unknown  0L  0 sym\n\
            src/\n  watcher.rs  rust  900L  40 sym\n  context.rs  rust  300L  20 sym\n\
            Cargo.lock  unknown  500L  0 sym\n\
            README.md  markdown  173L  0 sym\n";
        let filtered = filter_codedb_tree(raw);

        assert!(!filtered.contains("indexed"));
        assert!(!filtered.contains(".eyes/"));
        assert!(!filtered.contains("conversation.jsonl"));
        assert!(!filtered.contains("target/"));
        assert!(!filtered.contains("Cargo.lock"));
        assert!(filtered.contains("src/"));
        assert!(filtered.contains("watcher.rs  rust  900L  40 sym"));
        assert!(filtered.contains("context.rs"));
        assert!(filtered.contains("README.md"));
    }

    #[test]
    fn filter_codedb_tree_keeps_nondir_ignored_name_collisions() {
        // A file literally named "target" (not "target/") is a row, not a dir
        // block; only the trailing-slash directory form triggers block skipping.
        let raw = "src/\n  lib.rs  rust  10L  1 sym\ntarget  unknown  3L  0 sym\n";
        let filtered = filter_codedb_tree(raw);
        assert!(filtered.contains("lib.rs"));
        // "target" with no slash is still ignored by name, but must not eat siblings.
        assert!(!filtered.contains("target  unknown"));
        assert!(filtered.contains("src/"));
    }

    #[test]
    fn cap_repo_package_truncates_with_marker() {
        let big = "x".repeat(10_000);
        let capped = cap_repo_package(&big, 1_000);
        assert!(capped.len() <= 1_000);
        assert!(capped.ends_with(REPO_PACKAGE_TRUNCATION_MARKER));

        let small = "small map";
        assert_eq!(cap_repo_package(small, 1_000), small);
    }

    #[test]
    fn cap_repo_package_respects_char_boundaries() {
        let multibyte = "é".repeat(1_000);
        let capped = cap_repo_package(&multibyte, 100);
        // Must be valid UTF-8 (no panic) and within budget.
        assert!(capped.len() <= 100);
        assert!(capped.ends_with(REPO_PACKAGE_TRUNCATION_MARKER));
    }
}
