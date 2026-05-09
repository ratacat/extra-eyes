use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use extra_eyes::claude_install;
use extra_eyes::codex_install;
use extra_eyes::codex_trust;
use extra_eyes::context::build_context;
use extra_eyes::delivery::{render_hook_messages_with_budget, DEFAULT_HOOK_OUTPUT_BUDGET_BYTES};
use extra_eyes::filewatch::FileSnapshot;
use extra_eyes::ipc::{send_request, Request, Response, PROTOCOL_VERSION};
use extra_eyes::paths::ProjectPaths;
use extra_eyes::pi_install;
use extra_eyes::profiles::{self, ResolvedProfile};
use extra_eyes::watcher::WatcherContext;
use extra_eyes::{EyesError, Result};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "eyes",
    about = "Run Extra Eyes watcher workflows and harness hooks"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Resolve watcher profiles")]
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    #[command(about = "Send a watcher-style message to the daemon")]
    Message {
        #[command(subcommand)]
        command: MessageCommand,
    },
    #[command(about = "Record harness conversation traffic")]
    Feed {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Harness name: claude-code, codex, or pi")]
        harness: String,
        #[arg(long, help = "Harness event name")]
        event: String,
        #[arg(long, help = "Event payload as JSON")]
        payload_json: String,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Run harness hook adapters")]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    #[command(about = "Install harness integration files")]
    #[command(alias = "setup")]
    Install {
        #[command(subcommand)]
        command: InstallCommand,
    },
    #[command(about = "Run a watcher through the daemon")]
    Watcher {
        #[command(subcommand)]
        command: WatcherCommand,
    },
    #[command(about = "Run one watcher tick now")]
    Tick {
        #[arg(help = "Watcher profile names; defaults to the selected default profile")]
        profiles: Vec<String>,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Stable tick id for logs and watcher messages")]
        tick_id: Option<String>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Watch files and run watcher ticks on changes")]
    Watch {
        #[arg(help = "Watcher profile names; defaults to the selected default profile")]
        profiles: Vec<String>,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, default_value_t = 250, help = "Filesystem polling interval")]
        poll_ms: u64,
        #[arg(long, default_value_t = 250, help = "Quiet period before a tick runs")]
        debounce_ms: u64,
        #[arg(long, help = "Exit after this many ticks")]
        max_ticks: Option<u64>,
        #[arg(
            long,
            help = "Exit if no file changes arrive within this many milliseconds"
        )]
        idle_timeout_ms: Option<u64>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileCommand {
    #[command(about = "Show the profile selected by name or default precedence")]
    Resolve {
        #[arg(help = "Watcher profile name; omitted means the selected default")]
        profile: Option<String>,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MessageCommand {
    #[command(about = "Queue a synthetic watcher message")]
    Send {
        #[arg(help = "Message text")]
        text: String,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, default_value = "hook", help = "Delivery channel")]
        channel: String,
        #[arg(long, help = "Watcher name to show on the message")]
        watcher: String,
        #[arg(long, default_value = "info", help = "Message severity")]
        severity: String,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum InstallCommand {
    #[command(about = "Install Claude Code hooks")]
    ClaudeCode {
        #[arg(
            long,
            help = "Claude Code settings.json path; defaults to CLAUDE_CONFIG_DIR or ~/.claude"
        )]
        settings: Option<PathBuf>,
        #[arg(long, help = "Path to the eyes binary used by installed hooks")]
        eyes_bin: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Install Codex hooks and trust entries")]
    Codex {
        #[arg(
            long,
            help = "Codex config.toml path; defaults to CODEX_HOME/config.toml or ~/.codex/config.toml"
        )]
        config: Option<PathBuf>,
        #[arg(long, help = "Path to the eyes binary used by installed hooks")]
        eyes_bin: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Install a project-local pi extension")]
    Pi {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(
            long,
            help = "Extension file to write; defaults to <project>/.pi/extensions/extra-eyes.ts"
        )]
        extension_path: Option<PathBuf>,
        #[arg(long, help = "Path to the eyes binary used by the extension")]
        eyes_bin: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    #[command(about = "Fetch pending watcher messages for a harness session")]
    Fetch {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, default_value = "hook", help = "Delivery channel")]
        channel: String,
        #[arg(long, help = "Cursor key for the receiving session")]
        cursor_key: String,
        #[arg(long, help = "Maximum messages to fetch")]
        limit: Option<u32>,
    },
    #[command(about = "Compute or write Codex hook trust entries")]
    TrustCodex {
        #[arg(long, help = "Codex config containing hooks to trust")]
        hooks_config: PathBuf,
        #[arg(long, help = "Codex config where trust state should be written")]
        state_config: Option<PathBuf>,
        #[arg(long, help = "Write trust entries instead of only printing them")]
        write: bool,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Claude Code hook entrypoint installed by Extra Eyes")]
    ClaudeCode {
        #[arg(long, help = "Integration marker; installed hooks pass extra-eyes")]
        integration: Option<String>,
        #[arg(long, help = "Claude Code hook event name")]
        event: String,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, default_value = "hook", help = "Delivery channel")]
        channel: String,
        #[arg(long, help = "Maximum messages to fetch")]
        limit: Option<u32>,
    },
    #[command(about = "Codex hook entrypoint installed by Extra Eyes")]
    Codex {
        #[arg(long, help = "Integration marker; installed hooks pass extra-eyes")]
        integration: Option<String>,
        #[arg(long, help = "Codex hook event name")]
        event: String,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, default_value = "hook", help = "Delivery channel")]
        channel: String,
        #[arg(long, help = "Maximum messages to fetch")]
        limit: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
enum WatcherCommand {
    #[command(about = "Ask the daemon to run one watcher profile")]
    Run {
        #[arg(help = "Watcher profile name; omitted means the selected default")]
        profile: Option<String>,
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Stable tick id for logs and watcher messages")]
        tick_id: String,
        #[arg(long, help = "WatcherContext JSON; defaults to an empty context")]
        context_json: Option<String>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Profile {
            command:
                ProfileCommand::Resolve {
                    profile,
                    project,
                    json,
                },
        } => {
            let resolved = profiles::resolve_profile(project.as_deref(), profile.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resolved)?);
            } else {
                println!("{} ({:?})", resolved.profile.name, resolved.source);
            }
            Ok(())
        }
        Command::Message {
            command:
                MessageCommand::Send {
                    text,
                    project,
                    channel,
                    watcher,
                    severity,
                    json,
                },
        } => {
            let response = send_to_project(
                project.as_deref(),
                &Request::EnqueueMessage {
                    protocol: PROTOCOL_VERSION,
                    channel,
                    payload: serde_json::json!({
                        "watcher": watcher,
                        "severity": severity,
                        "text": text,
                    }),
                },
            )?;
            match response {
                Response::MessageEnqueued { message_id, .. } => {
                    if json {
                        println!("{}", serde_json::json!({"message_id": message_id}));
                    } else {
                        println!("{message_id}");
                    }
                    Ok(())
                }
                Response::Error { code, message, .. } => {
                    Err(EyesError::Protocol(format!("{code}: {message}")))
                }
                other => Err(EyesError::Protocol(format!(
                    "unexpected enqueue response: {other:?}"
                ))),
            }
        }
        Command::Hook {
            command:
                HookCommand::Fetch {
                    project,
                    channel,
                    cursor_key,
                    limit,
                },
        } => fetch_hook_messages(project, channel, cursor_key, limit),
        Command::Hook {
            command:
                HookCommand::TrustCodex {
                    hooks_config,
                    state_config,
                    write,
                    json,
                },
        } => trust_codex_hooks(hooks_config, state_config, write, json),
        Command::Hook {
            command:
                HookCommand::ClaudeCode {
                    integration: _,
                    event,
                    project,
                    channel,
                    limit,
                },
        } => run_claude_code_hook(project, event, channel, limit),
        Command::Hook {
            command:
                HookCommand::Codex {
                    integration: _,
                    event,
                    project,
                    channel,
                    limit,
                },
        } => run_codex_hook(project, event, channel, limit),
        Command::Install {
            command:
                InstallCommand::ClaudeCode {
                    settings,
                    eyes_bin,
                    json,
                },
        } => install_claude_code(settings, eyes_bin, json),
        Command::Install {
            command:
                InstallCommand::Codex {
                    config,
                    eyes_bin,
                    json,
                },
        } => install_codex(config, eyes_bin, json),
        Command::Install {
            command:
                InstallCommand::Pi {
                    project,
                    extension_path,
                    eyes_bin,
                    json,
                },
        } => install_pi(project, extension_path, eyes_bin, json),
        Command::Feed {
            project,
            harness,
            event,
            payload_json,
            json,
        } => record_conversation(project, harness, event, payload_json, json),
        Command::Watcher {
            command:
                WatcherCommand::Run {
                    profile,
                    project,
                    tick_id,
                    context_json,
                    json,
                },
        } => {
            let context = match context_json {
                Some(text) => serde_json::from_str::<WatcherContext>(&text)?,
                None => WatcherContext::default(),
            };
            let response = send_to_project(
                project.as_deref(),
                &Request::RunWatcher {
                    protocol: PROTOCOL_VERSION,
                    profile,
                    tick_id,
                    context,
                },
            )?;
            match response {
                Response::WatcherRun { .. } if json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                    Ok(())
                }
                Response::WatcherRun {
                    watcher,
                    message_ids,
                    statuses,
                    ..
                } => {
                    println!(
                        "{} messages={} statuses={}",
                        watcher,
                        message_ids.len(),
                        statuses.len()
                    );
                    Ok(())
                }
                Response::Error { code, message, .. } => {
                    Err(EyesError::Protocol(format!("{code}: {message}")))
                }
                other => Err(EyesError::Protocol(format!(
                    "unexpected watcher response: {other:?}"
                ))),
            }
        }
        Command::Tick {
            profiles,
            project,
            tick_id,
            json,
        } => {
            let paths = ProjectPaths::resolve(project.as_deref())?;
            let responses =
                run_tick_for_paths(&paths, profiles, tick_id.unwrap_or_else(default_tick_id))?;
            print_tick_responses(responses, json)
        }
        Command::Watch {
            profiles,
            project,
            poll_ms,
            debounce_ms,
            max_ticks,
            idle_timeout_ms,
            json,
        } => watch(
            project,
            profiles,
            poll_ms,
            debounce_ms,
            max_ticks,
            idle_timeout_ms,
            json,
        ),
    }
}

fn install_claude_code(
    settings: Option<PathBuf>,
    eyes_bin: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let settings_path = match settings {
        Some(path) => path,
        None => default_claude_settings_path()?,
    };
    let eyes_bin = match eyes_bin {
        Some(path) => path,
        None => std::env::current_exe()?,
    };
    let result = claude_install::install_claude_hooks(&settings_path, &eyes_bin)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "installed Extra Eyes Claude Code hooks in {}",
            result.settings_path.display()
        );
    }
    Ok(())
}

fn default_claude_settings_path() -> Result<PathBuf> {
    if let Some(claude_config_dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(claude_config_dir).join("settings.json"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".claude/settings.json"))
        .ok_or_else(|| {
            EyesError::Config(
                "cannot locate Claude Code settings: set CLAUDE_CONFIG_DIR, HOME, or pass --settings"
                    .to_owned(),
            )
        })
}

fn install_pi(
    project: Option<PathBuf>,
    extension_path: Option<PathBuf>,
    eyes_bin: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let paths = ProjectPaths::resolve(project.as_deref())?;
    let extension_path = extension_path
        .unwrap_or_else(|| paths.identity().root().join(".pi/extensions/extra-eyes.ts"));
    let eyes_bin = match eyes_bin {
        Some(path) => path,
        None => std::env::current_exe()?,
    };
    let result = pi_install::install_pi_extension(&extension_path, &eyes_bin)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "installed Extra Eyes pi extension at {}",
            result.extension_path.display()
        );
    }
    Ok(())
}

fn install_codex(config: Option<PathBuf>, eyes_bin: Option<PathBuf>, json: bool) -> Result<()> {
    let config_path = match config {
        Some(path) => path,
        None => default_codex_config_path()?,
    };
    let eyes_bin = match eyes_bin {
        Some(path) => path,
        None => std::env::current_exe()?,
    };
    let result = codex_install::install_codex_hooks(&config_path, &eyes_bin)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "installed Extra Eyes Codex hooks in {} and trusted {} hooks",
            result.config_path.display(),
            result.trust_entries.len()
        );
    }
    Ok(())
}

fn default_codex_config_path() -> Result<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("config.toml"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".codex/config.toml"))
        .ok_or_else(|| {
            EyesError::Config(
                "cannot locate Codex config: set CODEX_HOME, HOME, or pass --config".to_owned(),
            )
        })
}

fn trust_codex_hooks(
    hooks_config: PathBuf,
    state_config: Option<PathBuf>,
    write: bool,
    json: bool,
) -> Result<()> {
    let entries = codex_trust::trust_entries_for_config(&hooks_config)?;
    let state_config = state_config.unwrap_or_else(|| hooks_config.clone());
    if write {
        codex_trust::write_trust_state(&state_config, &entries)?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks_config": hooks_config,
                "state_config": state_config,
                "written": write,
                "entries": entries,
            }))?
        );
        return Ok(());
    }

    if write {
        println!(
            "trusted {} Codex hooks in {}",
            entries.len(),
            state_config.display()
        );
    } else {
        println!(
            "computed {} Codex hook trust entries for {}",
            entries.len(),
            hooks_config.display()
        );
    }
    for entry in entries {
        println!("{} {}", entry.key, entry.trusted_hash);
    }
    Ok(())
}

fn run_claude_code_hook(
    project: Option<PathBuf>,
    event: String,
    channel: String,
    limit: Option<u32>,
) -> Result<()> {
    let mut payload_json = String::new();
    if io::stdin().read_to_string(&mut payload_json).is_err() {
        return Ok(());
    }
    let payload = if payload_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str::<serde_json::Value>(&payload_json) {
            Ok(payload) => payload,
            Err(_) => return Ok(()),
        }
    };
    let normalized_event = normalize_hook_event_name(&event);

    if matches!(normalized_event.as_str(), "userpromptsubmit" | "stop") {
        let response = send_to_project(
            project.as_deref(),
            &Request::RecordConversation {
                protocol: PROTOCOL_VERSION,
                harness: "claude-code".to_owned(),
                event: event.clone(),
                payload: payload.clone(),
            },
        );
        if matches!(response, Err(EyesError::NotRunning)) {
            return Ok(());
        }
    }

    if normalized_event == "userpromptsubmit" {
        let Some(session_id) = extract_hook_session_id(&payload) else {
            return Ok(());
        };
        if let Some(additional_context) = fetch_and_commit_hook_messages(
            project,
            channel,
            format!("claude-code:{session_id}:hook"),
            limit,
        )? {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "UserPromptSubmit",
                        "additionalContext": additional_context,
                    }
                }))?
            );
        }
    }

    Ok(())
}

fn run_codex_hook(
    project: Option<PathBuf>,
    event: String,
    channel: String,
    limit: Option<u32>,
) -> Result<()> {
    let mut payload_json = String::new();
    if io::stdin().read_to_string(&mut payload_json).is_err() {
        return Ok(());
    }
    let payload = if payload_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str::<serde_json::Value>(&payload_json) {
            Ok(payload) => payload,
            Err(_) => return Ok(()),
        }
    };
    let normalized_event = normalize_hook_event_name(&event);

    if matches!(normalized_event.as_str(), "userpromptsubmit" | "stop") {
        let response = send_to_project(
            project.as_deref(),
            &Request::RecordConversation {
                protocol: PROTOCOL_VERSION,
                harness: "codex".to_owned(),
                event: event.clone(),
                payload: payload.clone(),
            },
        );
        if matches!(response, Err(EyesError::NotRunning)) {
            return Ok(());
        }
    }

    if matches!(
        normalized_event.as_str(),
        "sessionstart" | "userpromptsubmit"
    ) {
        let Some(session_id) = extract_hook_session_id(&payload) else {
            return Ok(());
        };
        if let Some(additional_context) = fetch_and_commit_hook_messages(
            project,
            channel,
            format!("codex:{session_id}:hook"),
            limit,
        )? {
            let hook_event_name = codex_context_hook_event_name(&normalized_event)
                .expect("normalized event was checked above");
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": hook_event_name,
                        "additionalContext": additional_context,
                    }
                }))?
            );
        }
        return Ok(());
    }

    Ok(())
}

fn normalize_hook_event_name(event: &str) -> String {
    event
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn extract_hook_session_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("session_id")
        .or_else(|| payload.get("sessionId"))
        .or_else(|| payload.get("session"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn codex_context_hook_event_name(normalized_event: &str) -> Option<&'static str> {
    match normalized_event {
        "sessionstart" => Some("SessionStart"),
        "userpromptsubmit" => Some("UserPromptSubmit"),
        _ => None,
    }
}

fn record_conversation(
    project: Option<PathBuf>,
    harness: String,
    event: String,
    payload_json: String,
    json: bool,
) -> Result<()> {
    let payload = serde_json::from_str(&payload_json)?;
    let response = send_to_project(
        project.as_deref(),
        &Request::RecordConversation {
            protocol: PROTOCOL_VERSION,
            harness,
            event,
            payload,
        },
    )?;
    match response {
        Response::ConversationRecorded { event_id, .. } => {
            if json {
                println!("{}", serde_json::json!({"event_id": event_id}));
            } else {
                println!("{event_id}");
            }
            Ok(())
        }
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected conversation response: {other:?}"
        ))),
    }
}

fn fetch_hook_messages(
    project: Option<PathBuf>,
    channel: String,
    cursor_key: String,
    limit: Option<u32>,
) -> Result<()> {
    let Some(rendered) = fetch_and_commit_hook_messages(project, channel, cursor_key, limit)?
    else {
        return Ok(());
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn fetch_and_commit_hook_messages(
    project: Option<PathBuf>,
    channel: String,
    cursor_key: String,
    limit: Option<u32>,
) -> Result<Option<String>> {
    let response = match send_to_project(
        project.as_deref(),
        &Request::FetchMessages {
            protocol: PROTOCOL_VERSION,
            channel: channel.clone(),
            cursor_key: cursor_key.clone(),
            limit,
        },
    ) {
        Ok(response) => response,
        Err(EyesError::NotRunning) => return Ok(None),
        Err(error) => return Err(error),
    };
    let messages = match response {
        Response::Messages { messages, .. } => messages,
        Response::Error { code, message, .. } => {
            return Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => {
            return Err(EyesError::Protocol(format!(
                "unexpected fetch response: {other:?}"
            )))
        }
    };

    if messages.is_empty() {
        return Ok(None);
    }

    let rendered = render_hook_messages_with_budget(&messages, DEFAULT_HOOK_OUTPUT_BUDGET_BYTES);
    let Some(through_message_id) = rendered.through_message_id else {
        return Ok(None);
    };

    match send_to_project(
        project.as_deref(),
        &Request::CommitCursor {
            protocol: PROTOCOL_VERSION,
            channel,
            cursor_key,
            through_message_id,
        },
    )? {
        Response::CursorCommitted { .. } => Ok(Some(rendered.text)),
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected cursor commit response: {other:?}"
        ))),
    }
}

fn watch(
    project: Option<PathBuf>,
    profiles: Vec<String>,
    poll_ms: u64,
    debounce_ms: u64,
    max_ticks: Option<u64>,
    idle_timeout_ms: Option<u64>,
    json: bool,
) -> Result<()> {
    if poll_ms == 0 {
        return Err(EyesError::Config(
            "poll_ms must be greater than zero".to_owned(),
        ));
    }
    let paths = ProjectPaths::resolve(project.as_deref())?;
    let root = paths.identity().root().to_path_buf();
    let selected = resolve_selected_profiles(&paths, profiles)?;
    let mut snapshot = FileSnapshot::scan(&root)?;
    let started = Instant::now();
    let mut ticks = 0_u64;

    loop {
        thread::sleep(Duration::from_millis(poll_ms));
        let next = FileSnapshot::scan(&root)?;
        if snapshot.has_changed(&next) {
            snapshot = settle_snapshot(&root, next, debounce_ms)?;
            ticks += 1;
            let responses =
                run_tick_for_resolved(&paths, &selected, default_tick_id(), Some(ticks))?;
            print_tick_responses(responses, json)?;
            if max_ticks.is_some_and(|max| ticks >= max) {
                return Ok(());
            }
        } else if idle_timeout_ms
            .map(|timeout| started.elapsed() >= Duration::from_millis(timeout))
            .unwrap_or(false)
        {
            return Ok(());
        }
    }
}

fn settle_snapshot(
    root: &std::path::Path,
    mut snapshot: FileSnapshot,
    debounce_ms: u64,
) -> Result<FileSnapshot> {
    if debounce_ms == 0 {
        return Ok(snapshot);
    }
    loop {
        thread::sleep(Duration::from_millis(debounce_ms));
        let next = FileSnapshot::scan(root)?;
        if !snapshot.has_changed(&next) {
            return Ok(next);
        }
        snapshot = next;
    }
}

fn run_tick_for_paths(
    paths: &ProjectPaths,
    profiles: Vec<String>,
    tick_id: String,
) -> Result<Vec<Response>> {
    let selected = resolve_selected_profiles(paths, profiles)?;
    run_tick_for_resolved(paths, &selected, tick_id, None)
}

fn run_tick_for_resolved(
    paths: &ProjectPaths,
    profiles: &[ResolvedProfile],
    tick_id: String,
    scheduler_tick: Option<u64>,
) -> Result<Vec<Response>> {
    let context = build_context(paths)?;
    let mut responses = Vec::new();
    for resolved in profiles {
        if let Some(tick_no) = scheduler_tick {
            let cadence = profile_cadence_ticks(resolved)?;
            if (tick_no - 1) % cadence != 0 {
                continue;
            }
        }
        responses.push(send_request(
            paths.socket_path(),
            &Request::RunWatcher {
                protocol: PROTOCOL_VERSION,
                profile: Some(resolved.profile.name.clone()),
                tick_id: tick_id.clone(),
                context: context.clone(),
            },
        )?);
    }
    Ok(responses)
}

fn resolve_selected_profiles(
    paths: &ProjectPaths,
    names: Vec<String>,
) -> Result<Vec<ResolvedProfile>> {
    if names.is_empty() {
        return Ok(vec![profiles::resolve_profile(
            Some(paths.identity().root()),
            None,
        )?]);
    }
    names
        .iter()
        .map(|name| profiles::resolve_profile(Some(paths.identity().root()), Some(name)))
        .collect()
}

fn profile_cadence_ticks(resolved: &ResolvedProfile) -> Result<u64> {
    let cadence = profiles::integer_setting(&resolved.profile, "cadence_ticks")?.unwrap_or(1);
    if cadence == 0 {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.cadence_ticks must be greater than zero",
            resolved.profile.name
        )));
    }
    Ok(cadence)
}

fn print_tick_responses(responses: Vec<Response>, json: bool) -> Result<()> {
    if json {
        if responses.len() == 1 {
            println!("{}", serde_json::to_string_pretty(&responses[0])?);
        } else {
            println!("{}", serde_json::to_string_pretty(&responses)?);
        }
        return Ok(());
    }

    for response in responses {
        print_tick_response(response)?;
    }
    Ok(())
}

fn print_tick_response(response: Response) -> Result<()> {
    match response {
        Response::WatcherRun {
            watcher,
            message_ids,
            statuses,
            ..
        } => {
            println!(
                "{} messages={} statuses={}",
                watcher,
                message_ids.len(),
                statuses.len()
            );
            Ok(())
        }
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected tick response: {other:?}"
        ))),
    }
}

fn send_to_project(project: Option<&std::path::Path>, request: &Request) -> Result<Response> {
    let paths = ProjectPaths::resolve(project)?;
    send_request(paths.socket_path(), request)
}

fn default_tick_id() -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_millis();
    format!("tick-{now_ms}")
}
