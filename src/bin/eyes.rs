use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use extra_eyes::claude_install;
use extra_eyes::codex_install;
use extra_eyes::codex_trust;
use extra_eyes::context::build_context;
use extra_eyes::daemon;
use extra_eyes::delivery::{
    render_compact_hook_messages_with_budget, render_hook_messages_with_budget,
    DEFAULT_HOOK_OUTPUT_BUDGET_BYTES,
};
use extra_eyes::filewatch::FileSnapshot;
use extra_eyes::ipc::{
    send_request, IpcMessage, Request, Response, WatcherRunSummary, PROTOCOL_VERSION,
};
use extra_eyes::paths::{runtime_base_dir, ProjectPaths};
use extra_eyes::pi_install;
use extra_eyes::profiles::{self, ResolvedProfile};
use extra_eyes::state::{ConversationRecord, StateStore};
use extra_eyes::terminal::{self, ColorChoice, Style};
use extra_eyes::watcher::{WatcherContext, WatcherMessage, WatcherRef, WatcherStatusEvent};
use extra_eyes::{EyesError, Result};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DIRECT_MENTION_HOOK_WAIT_MS: u64 = 15_000;
const HOOK_WAIT_POLL_MS: u64 = 100;

#[derive(Debug, Parser)]
#[command(
    name = "eyes",
    about = "Run Extra Eyes watcher workflows and harness hooks"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorChoice::Auto,
        help = "When to use ANSI color: auto, always, or never"
    )]
    color: ColorChoice,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Start the project daemon")]
    Start {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Show project daemon status")]
    Status {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Stop the project daemon")]
    Stop {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
    },
    #[command(about = "Restart the project daemon")]
    Restart {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(hide = true, about = "Internal daemon entrypoints")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
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
enum DaemonCommand {
    #[command(hide = true, about = "Run the daemon in the foreground")]
    Foreground {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
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
        #[arg(
            long,
            default_value_t = 0,
            help = "Wait up to this many milliseconds for a message before returning"
        )]
        wait_ms: u64,
        #[arg(long, default_value_t = 100, help = "Polling interval while waiting")]
        wait_poll_ms: u64,
        #[arg(
            long,
            help = "When waiting, ignore messages already queued at fetch start"
        )]
        fresh: bool,
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
    let stdout_style = Style::stdout(cli.color);
    let stderr_style = Style::stderr(cli.color);
    let command = cli.command.unwrap_or_else(default_command);
    match command {
        Command::Start { project, json } => start_daemon(project, json, &stdout_style),
        Command::Status { project, json } => print_daemon_status(project, json, &stdout_style),
        Command::Stop { project } => stop_daemon(project, &stdout_style),
        Command::Restart { project, json } => restart_daemon(project, json, &stdout_style),
        Command::Daemon {
            command: DaemonCommand::Foreground { project },
        } => daemon::start_foreground(project.as_deref()),
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
                println!(
                    "{}",
                    stdout_style.line(
                        "eyes",
                        "profile",
                        "ready",
                        terminal::details(&[
                            ("name", resolved.profile.name.clone()),
                            ("source", profile_source_label(&resolved).to_owned()),
                        ]),
                    )
                );
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
                        println!(
                            "{}",
                            stdout_style.line(
                                "eyes",
                                "message",
                                "queued",
                                terminal::details(&[("id", message_id.to_string())]),
                            )
                        );
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
                    wait_ms,
                    wait_poll_ms,
                    fresh,
                },
        } => fetch_hook_messages(
            project,
            channel,
            cursor_key,
            limit,
            wait_ms,
            wait_poll_ms,
            fresh,
        ),
        Command::Hook {
            command:
                HookCommand::TrustCodex {
                    hooks_config,
                    state_config,
                    write,
                    json,
                },
        } => trust_codex_hooks(hooks_config, state_config, write, json, &stdout_style),
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
        } => install_claude_code(settings, eyes_bin, json, &stdout_style),
        Command::Install {
            command:
                InstallCommand::Codex {
                    config,
                    eyes_bin,
                    json,
                },
        } => install_codex(config, eyes_bin, json, &stdout_style),
        Command::Install {
            command:
                InstallCommand::Pi {
                    project,
                    extension_path,
                    eyes_bin,
                    json,
                },
        } => install_pi(project, extension_path, eyes_bin, json, &stdout_style),
        Command::Feed {
            project,
            harness,
            event,
            payload_json,
            json,
        } => record_conversation(project, harness, event, payload_json, json, &stdout_style),
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
                    target_session_id: None,
                },
            )?;
            match response {
                Response::WatcherRun { .. } if json => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                    Ok(())
                }
                Response::WatcherRun {
                    watcher,
                    summary,
                    messages,
                    statuses,
                    ..
                } => {
                    print_human_watcher_run(
                        &watcher,
                        &summary,
                        &messages,
                        &statuses,
                        &stdout_style,
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
            let responses = run_tick_for_paths(
                &paths,
                profiles,
                tick_id.unwrap_or_else(default_tick_id),
                json,
                &stderr_style,
            )?;
            print_tick_responses(responses, json, &stdout_style)
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
            &stdout_style,
            &stderr_style,
        ),
    }
}

fn start_daemon(project: Option<PathBuf>, json: bool, style: &Style) -> Result<()> {
    let started = daemon::start_detached(project.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        println!(
            "{}",
            style.line(
                "eyes",
                "daemon",
                "started",
                terminal::details(&[
                    ("pid", started.pid.to_string()),
                    ("project", started.project_root),
                    ("log", started.log_path.display().to_string()),
                ]),
            )
        );
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
struct DaemonStatusRow {
    pid: u32,
    project_root: String,
    project_hash: String,
    socket_path: String,
    state_dir: String,
    current: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HarnessHookCoverage {
    harness: String,
    installed: bool,
    path: Option<String>,
    events: Vec<HookEventCoverage>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HookEventCoverage {
    event: String,
    installed: bool,
}

fn print_daemon_status(project: Option<PathBuf>, json: bool, style: &Style) -> Result<()> {
    let paths = ProjectPaths::resolve(project.as_deref())?;
    let current_root = paths.identity().root_string().to_owned();
    let daemons = discover_running_daemons(&current_root)?;
    let current_daemon = daemons.iter().find(|daemon| daemon.current).cloned();
    let hook_coverage = hook_coverages(&paths);

    if json {
        let output = if let Some(daemon) = current_daemon {
            serde_json::json!({
                "status": "running",
                "pid": daemon.pid,
                "project_root": daemon.project_root,
                "project_hash": daemon.project_hash,
                "socket_path": daemon.socket_path,
                "state_dir": daemon.state_dir,
                "current_project_root": current_root,
                "current_pid": daemon.pid,
                "runtime_base": runtime_base_dir(),
                "daemons": daemons,
                "hook_coverage": hook_coverage,
            })
        } else {
            serde_json::json!({
                "status": "not_running",
                "current_project_root": current_root,
                "current_pid": null,
                "runtime_base": runtime_base_dir(),
                "daemons": daemons,
                "hook_coverage": hook_coverage,
            })
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    print_status_page(&current_root, &daemons, &hook_coverage, style);
    Ok(())
}

fn discover_running_daemons(current_root: &str) -> Result<Vec<DaemonStatusRow>> {
    let runtime_base = runtime_base_dir();
    let entries = match fs::read_dir(&runtime_base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut daemons = Vec::new();
    for entry in entries {
        let entry = entry?;
        let socket_path = entry.path().join("eyesd.sock");
        if !socket_path.exists() {
            continue;
        }
        match send_request(
            &socket_path,
            &Request::Status {
                protocol: PROTOCOL_VERSION,
            },
        ) {
            Ok(Response::Status {
                pid,
                project_root,
                project_hash,
                socket_path,
                state_dir,
                ..
            }) => daemons.push(DaemonStatusRow {
                pid,
                current: project_root == current_root,
                project_root,
                project_hash,
                socket_path,
                state_dir,
            }),
            Ok(Response::Error { .. }) | Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => {}
            Ok(other) => {
                return Err(EyesError::Protocol(format!(
                    "unexpected daemon status response: {other:?}"
                )))
            }
            Err(error) => return Err(error),
        }
    }

    daemons.sort_by(|left, right| {
        right
            .current
            .cmp(&left.current)
            .then_with(|| left.project_root.cmp(&right.project_root))
    });
    Ok(daemons)
}

fn hook_coverages(paths: &ProjectPaths) -> Vec<HarnessHookCoverage> {
    vec![
        codex_hook_coverage(),
        claude_hook_coverage(),
        pi_hook_coverage(paths),
    ]
}

fn codex_hook_coverage() -> HarnessHookCoverage {
    let path = match default_codex_config_path() {
        Ok(path) => path,
        Err(error) => {
            return missing_coverage(
                "Codex",
                None,
                &codex_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return missing_coverage(
                "Codex",
                Some(path),
                &codex_install::INSTALLED_EVENTS,
                vec![],
            );
        }
        Err(error) => {
            return missing_coverage(
                "Codex",
                Some(path),
                &codex_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let parsed = match contents.parse::<toml::Value>() {
        Ok(parsed) => parsed,
        Err(error) => {
            return missing_coverage(
                "Codex",
                Some(path),
                &codex_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let trust_entries = codex_trust::trust_entries_for_config(&path).unwrap_or_else(|_| Vec::new());
    let events = codex_install::INSTALLED_EVENTS
        .iter()
        .map(|event| {
            let command_installed = codex_event_command_installed(&parsed, event);
            let trust_installed = codex_event_trust_installed(&parsed, &trust_entries, event);
            HookEventCoverage {
                event: (*event).to_owned(),
                installed: command_installed && trust_installed,
            }
        })
        .collect::<Vec<_>>();
    coverage_from_events("Codex", Some(path), events, Vec::new())
}

fn claude_hook_coverage() -> HarnessHookCoverage {
    let path = match default_claude_settings_path() {
        Ok(path) => path,
        Err(error) => {
            return missing_coverage(
                "Claude Code",
                None,
                &claude_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return missing_coverage(
                "Claude Code",
                Some(path),
                &claude_install::INSTALLED_EVENTS,
                vec![],
            );
        }
        Err(error) => {
            return missing_coverage(
                "Claude Code",
                Some(path),
                &claude_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let parsed = match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            return missing_coverage(
                "Claude Code",
                Some(path),
                &claude_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let events = claude_install::INSTALLED_EVENTS
        .iter()
        .map(|event| HookEventCoverage {
            event: (*event).to_owned(),
            installed: claude_event_command_installed(&parsed, event),
        })
        .collect::<Vec<_>>();
    coverage_from_events("Claude Code", Some(path), events, Vec::new())
}

fn pi_hook_coverage(paths: &ProjectPaths) -> HarnessHookCoverage {
    let path = paths.identity().root().join(".pi/extensions/extra-eyes.ts");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return missing_coverage("pi", Some(path), &pi_install::INSTALLED_EVENTS, vec![]);
        }
        Err(error) => {
            return missing_coverage(
                "pi",
                Some(path),
                &pi_install::INSTALLED_EVENTS,
                vec![error.to_string()],
            );
        }
    };
    let events = pi_install::INSTALLED_EVENTS
        .iter()
        .map(|event| HookEventCoverage {
            event: (*event).to_owned(),
            installed: contents.contains(&format!("pi.on(\"{event}\""))
                && contents.contains("hook")
                && contents.contains("fetch"),
        })
        .collect::<Vec<_>>();
    coverage_from_events("pi", Some(path), events, Vec::new())
}

fn missing_coverage(
    harness: &str,
    path: Option<PathBuf>,
    expected_events: &[&str],
    warnings: Vec<String>,
) -> HarnessHookCoverage {
    coverage_from_events(
        harness,
        path,
        expected_events
            .iter()
            .map(|event| HookEventCoverage {
                event: (*event).to_owned(),
                installed: false,
            })
            .collect(),
        warnings,
    )
}

fn coverage_from_events(
    harness: &str,
    path: Option<PathBuf>,
    events: Vec<HookEventCoverage>,
    warnings: Vec<String>,
) -> HarnessHookCoverage {
    HarnessHookCoverage {
        harness: harness.to_owned(),
        installed: events.iter().all(|event| event.installed),
        path: path.map(|path| path.display().to_string()),
        events,
        warnings,
    }
}

fn codex_event_command_installed(root: &toml::Value, event: &str) -> bool {
    root.get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .any(|group| {
            group
                .get("hooks")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .any(|hook| {
                    hook.get("command")
                        .and_then(toml::Value::as_str)
                        .map(|command| {
                            command.contains(" hook codex ")
                                && command.contains(" --integration extra-eyes ")
                                && command.contains(&format!(" --event {event} "))
                        })
                        .unwrap_or(false)
                })
        })
}

fn codex_event_trust_installed(
    root: &toml::Value,
    trust_entries: &[codex_trust::CodexHookTrustEntry],
    event: &str,
) -> bool {
    trust_entries
        .iter()
        .filter(|entry| {
            entry.event_name == event
                && entry.command.contains(" hook codex ")
                && entry.command.contains(" --integration extra-eyes ")
        })
        .any(|entry| {
            root.get("hooks")
                .and_then(|hooks| hooks.get("state"))
                .and_then(|state| state.get(&entry.key))
                .and_then(|trust| trust.get("trusted_hash"))
                .and_then(toml::Value::as_str)
                == Some(entry.trusted_hash.as_str())
        })
}

fn claude_event_command_installed(root: &serde_json::Value, event: &str) -> bool {
    root.get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|group| {
            group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|hook| {
                    hook.get("command")
                        .and_then(serde_json::Value::as_str)
                        .map(|command| {
                            command.contains(" hook claude-code ")
                                && command.contains(" --integration extra-eyes ")
                                && command.contains(&format!(" --event {event} "))
                        })
                        .unwrap_or(false)
                })
        })
}

fn print_status_page(
    current_root: &str,
    daemons: &[DaemonStatusRow],
    hook_coverage: &[HarnessHookCoverage],
    style: &Style,
) {
    println!("{}", style.brand(" _____ __   __ _____ ____"));
    println!("{}", style.brand("| ____|\\ \\ / /| ____/ ___|"));
    println!("{}", style.brand("|  _|   \\ V / |  _| \\___ \\"));
    println!("{}", style.brand("| |___   | |  | |___ ___) |"));
    println!("{}", style.brand("|_____|  |_|  |_____|____/"));
    println!();
    println!("Project-scoped watcher daemons");
    println!();
    println!("Current project");
    println!("  {current_root}");
    println!();
    println!("Running daemons");
    if daemons.is_empty() {
        println!("  none");
        println!();
        println!("Start one with `eyes` or `eyes start`.");
        println!();
        print_hook_coverage(hook_coverage, style);
        return;
    }

    let pid_width = daemons
        .iter()
        .map(|daemon| daemon.pid.to_string().len())
        .max()
        .unwrap_or(3)
        .max(3);
    println!("  {:1}  {:>pid_width$}  folder", "", "pid");
    for daemon in daemons {
        let marker = if daemon.current { "*" } else { " " };
        println!(
            "  {marker}  {:>pid_width$}  {}",
            daemon.pid, daemon.project_root
        );
    }
    println!();
    println!("* current project");
    println!();
    print_hook_coverage(hook_coverage, style);
}

fn print_hook_coverage(hook_coverage: &[HarnessHookCoverage], style: &Style) {
    for line in hook_coverage_lines(hook_coverage, style) {
        println!("{line}");
    }
}

fn eprint_hook_coverage(hook_coverage: &[HarnessHookCoverage], style: &Style) {
    for line in hook_coverage_lines(hook_coverage, style) {
        eprintln!("{line}");
    }
}

fn hook_coverage_lines(hook_coverage: &[HarnessHookCoverage], style: &Style) -> Vec<String> {
    let mut lines = vec!["Hook coverage".to_owned()];
    for coverage in hook_coverage {
        let icon = if coverage.installed {
            style.success("✓")
        } else {
            style.muted("×")
        };
        let missing = coverage
            .events
            .iter()
            .filter(|event| !event.installed)
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>();
        let details = if missing.is_empty() {
            format!(
                "events {}",
                coverage
                    .events
                    .iter()
                    .map(|event| event.event.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            format!("missing {}", missing.join(","))
        };
        lines.push(format!("  {icon} {:<11} {details}", coverage.harness));
        for warning in &coverage.warnings {
            lines.push(format!("    {}", style.muted(warning)));
        }
    }
    lines
}

fn stop_daemon(project: Option<PathBuf>, style: &Style) -> Result<()> {
    let response = daemon::stop(project.as_deref())?;
    match response {
        Response::Stopping { .. } => {
            println!("{}", style.line("eyes", "daemon", "stopping", ""));
            Ok(())
        }
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected stop response: {other:?}"
        ))),
    }
}

fn restart_daemon(project: Option<PathBuf>, json: bool, style: &Style) -> Result<()> {
    let restarted = daemon::restart(project.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&restarted)?);
    } else {
        let verb = if restarted.stopped_existing {
            "restarted"
        } else {
            "started"
        };
        println!(
            "{}",
            style.line(
                "eyes",
                "daemon",
                verb,
                terminal::details(&[
                    ("pid", restarted.started.pid.to_string()),
                    ("project", restarted.started.project_root),
                    ("log", restarted.started.log_path.display().to_string()),
                ]),
            )
        );
    }
    Ok(())
}

fn install_claude_code(
    settings: Option<PathBuf>,
    eyes_bin: Option<PathBuf>,
    json: bool,
    style: &Style,
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
            "{}",
            style.line(
                "eyes",
                "install",
                "installed",
                terminal::details(&[
                    ("harness", "claude-code".to_owned()),
                    ("settings", result.settings_path.display().to_string()),
                ]),
            )
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
    style: &Style,
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
            "{}",
            style.line(
                "eyes",
                "install",
                "installed",
                terminal::details(&[
                    ("harness", "pi".to_owned()),
                    ("extension", result.extension_path.display().to_string()),
                ]),
            )
        );
    }
    Ok(())
}

fn install_codex(
    config: Option<PathBuf>,
    eyes_bin: Option<PathBuf>,
    json: bool,
    style: &Style,
) -> Result<()> {
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
            "{}",
            style.line(
                "eyes",
                "install",
                "installed",
                terminal::details(&[
                    ("harness", "codex".to_owned()),
                    ("config", result.config_path.display().to_string()),
                    ("trusted", result.trust_entries.len().to_string()),
                ]),
            )
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
    style: &Style,
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
            "{}",
            style.line(
                "eyes",
                "trust",
                "trusted",
                terminal::details(&[
                    ("hooks", entries.len().to_string()),
                    ("state", state_config.display().to_string()),
                ]),
            )
        );
    } else {
        println!(
            "{}",
            style.line(
                "eyes",
                "trust",
                "ready",
                terminal::details(&[
                    ("hooks", entries.len().to_string()),
                    ("config", hooks_config.display().to_string()),
                ]),
            )
        );
    }
    for entry in entries {
        println!(
            "{}",
            style.line(
                "eyes",
                "trust",
                "info",
                terminal::details(&[("key", entry.key), ("hash", entry.trusted_hash),]),
            )
        );
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
    let hook_session_id = if is_delivery_hook_event(&normalized_event) {
        extract_hook_session_id(&payload)
    } else {
        None
    };
    let fresh_after = if normalized_event == "userpromptsubmit" && payload_mentions_eyes(&payload) {
        if let Some(session_id) = &hook_session_id {
            latest_hook_message_id(
                project.clone(),
                channel.clone(),
                format!("claude-code:{session_id}:hook"),
            )?
        } else {
            None
        }
    } else {
        None
    };

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

    if is_delivery_hook_event(&normalized_event) {
        let Some(session_id) = hook_session_id else {
            return Ok(());
        };
        if let Some(delivery) = fetch_hook_delivery(
            project,
            channel,
            format!("claude-code:{session_id}:hook"),
            limit,
            hook_wait_ms(&normalized_event, &payload),
            HOOK_WAIT_POLL_MS,
            fresh_after,
            fresh_after.is_some(),
            false,
        )? {
            if !claim_hook_delivery(&delivery)? {
                return Ok(());
            }
            let hook_event_name = context_hook_event_name(&normalized_event)
                .expect("normalized event was checked above");
            let output = serde_json::to_string(&serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": delivery.text.clone(),
                }
            }))?;
            let mut stdout = io::stdout().lock();
            stdout.write_all(output.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
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
    let hook_session_id = if matches!(
        normalized_event.as_str(),
        "sessionstart" | "userpromptsubmit" | "pretooluse"
    ) {
        extract_hook_session_id(&payload)
    } else {
        None
    };
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

    if normalized_event == "pretooluse" {
        let Some(session_id) = hook_session_id else {
            return Ok(());
        };
        let cursor_key = format!("codex:{session_id}:hook");
        if let Some(delivery) = fetch_hook_delivery(
            project,
            channel,
            cursor_key,
            limit,
            0,
            HOOK_WAIT_POLL_MS,
            None,
            false,
            true,
        )? {
            if !claim_hook_delivery(&delivery)? {
                return Ok(());
            }
            let hook_event_name = context_hook_event_name(&normalized_event)
                .expect("normalized event was checked above");
            let output = serde_json::to_string(&serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": delivery.text.clone(),
                }
            }))?;
            let mut stdout = io::stdout().lock();
            stdout.write_all(output.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        return Ok(());
    }

    Ok(())
}

fn is_delivery_hook_event(normalized_event: &str) -> bool {
    matches!(normalized_event, "userpromptsubmit" | "pretooluse")
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

fn context_hook_event_name(normalized_event: &str) -> Option<&'static str> {
    match normalized_event {
        "sessionstart" => Some("SessionStart"),
        "userpromptsubmit" => Some("UserPromptSubmit"),
        "pretooluse" => Some("PreToolUse"),
        _ => None,
    }
}

fn hook_wait_ms(normalized_event: &str, payload: &serde_json::Value) -> u64 {
    if normalized_event == "userpromptsubmit" && payload_mentions_eyes(payload) {
        DIRECT_MENTION_HOOK_WAIT_MS
    } else {
        0
    }
}

fn payload_mentions_eyes(payload: &serde_json::Value) -> bool {
    payload_user_text(payload)
        .map(|text| text.to_ascii_lowercase().contains("@eyes"))
        .unwrap_or(false)
}

fn payload_user_text(payload: &serde_json::Value) -> Option<&str> {
    for key in ["prompt", "text", "message", "input"] {
        if let Some(text) = payload.get(key).and_then(serde_json::Value::as_str) {
            return Some(text);
        }
        if let Some(text) = payload
            .get("event")
            .and_then(|event| event.get(key))
            .and_then(serde_json::Value::as_str)
        {
            return Some(text);
        }
    }
    None
}

fn record_conversation(
    project: Option<PathBuf>,
    harness: String,
    event: String,
    payload_json: String,
    json: bool,
    style: &Style,
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
                println!(
                    "{}",
                    style.line(
                        "eyes",
                        "feed",
                        "queued",
                        terminal::details(&[("event", event_id.to_string())]),
                    )
                );
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
    wait_ms: u64,
    wait_poll_ms: u64,
    fresh: bool,
) -> Result<()> {
    let fresh_after = if fresh {
        latest_hook_message_id(project.clone(), channel.clone(), cursor_key.clone())?
    } else {
        None
    };
    let Some(delivery) = fetch_hook_delivery(
        project,
        channel,
        cursor_key,
        limit,
        wait_ms,
        wait_poll_ms,
        fresh_after,
        false,
        false,
    )?
    else {
        return Ok(());
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(delivery.text.as_bytes())?;
    stdout.flush()?;
    commit_hook_delivery(delivery)?;
    Ok(())
}

struct PendingHookDelivery {
    project: Option<PathBuf>,
    channel: String,
    cursor_key: String,
    expected_last_message_id: u64,
    through_message_id: Option<u64>,
    text: String,
}

fn fetch_hook_delivery(
    project: Option<PathBuf>,
    channel: String,
    cursor_key: String,
    limit: Option<u32>,
    wait_ms: u64,
    wait_poll_ms: u64,
    fresh_after: Option<u64>,
    fresh_targeted_only: bool,
    compact: bool,
) -> Result<Option<PendingHookDelivery>> {
    if wait_ms > 0 && wait_poll_ms == 0 {
        return Err(EyesError::Config(
            "wait_poll_ms must be greater than zero when wait_ms is set".to_owned(),
        ));
    }

    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut buffered = Vec::<IpcMessage>::new();
    let mut seen = std::collections::BTreeSet::<u64>::new();
    let (messages, mut expected_last_message_id) = loop {
        let batch = match fetch_message_batch(
            project.as_deref(),
            &channel,
            &cursor_key,
            limit,
            fresh_after,
            fresh_targeted_only,
            false,
        ) {
            Ok(batch) => batch,
            Err(EyesError::NotRunning) => return Ok(None),
            Err(error) => return Err(error),
        };
        let batch_last_message_id = batch.last_message_id;
        for message in batch.messages {
            if seen.insert(message.message_id) {
                buffered.push(message);
            }
        }
        let has_messages = !buffered.is_empty();
        let has_fresh = fresh_after
            .map(|threshold| {
                buffered
                    .iter()
                    .any(|message| message.message_id > threshold)
            })
            .unwrap_or(has_messages);
        if has_fresh || wait_ms == 0 || Instant::now() >= deadline {
            break (buffered, batch_last_message_id);
        }
        thread::sleep(
            Duration::from_millis(wait_poll_ms)
                .min(deadline.saturating_duration_since(Instant::now())),
        );
    };

    let mut messages_to_render = if fresh_targeted_only && !messages.is_empty() {
        match fetch_message_batch(
            project.as_deref(),
            &channel,
            &cursor_key,
            limit,
            None,
            false,
            false,
        ) {
            Ok(batch) => {
                expected_last_message_id = batch.last_message_id;
                batch.messages
            }
            Err(EyesError::NotRunning) => return Ok(None),
            Err(error) => return Err(error),
        }
    } else {
        messages
    };
    if let Some(threshold) = fresh_after.filter(|_| !fresh_targeted_only) {
        messages_to_render.retain(|message| message.message_id > threshold);
    }

    if messages_to_render.is_empty() {
        return Ok(None);
    }

    let rendered = if compact {
        render_compact_hook_messages_with_budget(
            &messages_to_render,
            DEFAULT_HOOK_OUTPUT_BUDGET_BYTES,
        )
    } else {
        render_hook_messages_with_budget(&messages_to_render, DEFAULT_HOOK_OUTPUT_BUDGET_BYTES)
    };
    Ok(Some(PendingHookDelivery {
        project,
        channel,
        cursor_key,
        expected_last_message_id,
        through_message_id: rendered.through_message_id,
        text: rendered.text,
    }))
}

fn claim_hook_delivery(delivery: &PendingHookDelivery) -> Result<bool> {
    let Some(through_message_id) = delivery.through_message_id else {
        return Ok(false);
    };
    match send_to_project(
        delivery.project.as_deref(),
        &Request::CommitCursorIfCurrent {
            protocol: PROTOCOL_VERSION,
            channel: delivery.channel.clone(),
            cursor_key: delivery.cursor_key.clone(),
            expected_last_message_id: delivery.expected_last_message_id,
            through_message_id,
        },
    )? {
        Response::CursorCommitted { .. } => Ok(true),
        Response::CursorCommitStale { .. } => Ok(false),
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected cursor claim response: {other:?}"
        ))),
    }
}

fn commit_hook_delivery(delivery: PendingHookDelivery) -> Result<()> {
    let Some(through_message_id) = delivery.through_message_id else {
        return Ok(());
    };
    commit_hook_cursor_if_possible(
        delivery.project.as_deref(),
        delivery.channel,
        delivery.cursor_key,
        through_message_id,
    )
}

fn commit_hook_cursor_if_possible(
    project: Option<&std::path::Path>,
    channel: String,
    cursor_key: String,
    through_message_id: u64,
) -> Result<()> {
    match send_to_project(
        project,
        &Request::CommitCursor {
            protocol: PROTOCOL_VERSION,
            channel,
            cursor_key,
            through_message_id,
        },
    )? {
        Response::CursorCommitted { .. } => Ok(()),
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected cursor commit response: {other:?}"
        ))),
    }
}

struct MessageBatch {
    last_message_id: u64,
    latest_message_id: u64,
    messages: Vec<IpcMessage>,
}

fn latest_hook_message_id(
    project: Option<PathBuf>,
    channel: String,
    cursor_key: String,
) -> Result<Option<u64>> {
    match fetch_message_batch(
        project.as_deref(),
        &channel,
        &cursor_key,
        Some(1),
        None,
        false,
        false,
    ) {
        Ok(batch) => Ok(Some(batch.latest_message_id)),
        Err(EyesError::NotRunning) => Ok(None),
        Err(error) => Err(error),
    }
}

fn fetch_message_batch(
    project: Option<&std::path::Path>,
    channel: &str,
    cursor_key: &str,
    limit: Option<u32>,
    after_message_id: Option<u64>,
    targeted_only: bool,
    include_all_targets: bool,
) -> Result<MessageBatch> {
    let response = match send_to_project(
        project,
        &Request::FetchMessages {
            protocol: PROTOCOL_VERSION,
            channel: channel.to_owned(),
            cursor_key: cursor_key.to_owned(),
            limit,
            after_message_id,
            targeted_only,
            include_all_targets,
        },
    ) {
        Ok(response) => response,
        Err(EyesError::NotRunning) => return Err(EyesError::NotRunning),
        Err(error) => return Err(error),
    };
    match response {
        Response::Messages {
            last_message_id,
            latest_message_id,
            messages,
            ..
        } => Ok(MessageBatch {
            last_message_id,
            latest_message_id,
            messages,
        }),
        Response::Error { code, message, .. } => {
            Err(EyesError::Protocol(format!("{code}: {message}")))
        }
        other => Err(EyesError::Protocol(format!(
            "unexpected fetch response: {other:?}"
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
    stdout_style: &Style,
    stderr_style: &Style,
) -> Result<()> {
    if poll_ms == 0 {
        return Err(EyesError::Config(
            "poll_ms must be greater than zero".to_owned(),
        ));
    }
    let paths = ProjectPaths::resolve(project.as_deref())?;
    let root = paths.identity().root().to_path_buf();
    let selected = resolve_selected_profiles(&paths, profiles)?;
    let warm_profiles = if max_ticks.is_none() && idle_timeout_ms.is_none() {
        warm_start_profiles(&selected)?
    } else {
        Vec::new()
    };
    let mut snapshot = FileSnapshot::scan(&root)?;
    let mut last_conversation_id = conversation_watermark(&paths)?;
    ensure_daemon_running(&paths, json, stderr_style)?;
    let watch_cursor_key = watch_cursor_key();
    let _ = ensure_watcher_check_ins(&paths, &selected)?;
    let check_in_ids =
        ensure_watcher_check_ins_for(&paths, &selected, Some(watch_cursor_key.as_str()))?;
    seed_watch_cursor(&paths, &watch_cursor_key, &check_in_ids)?;
    print_watch_started(&paths, &selected, &warm_profiles, json, stderr_style);
    let mut last_activity = Instant::now();
    let mut printed_watch_message = false;
    if !json {
        eprint_hook_coverage(&hook_coverages(&paths), stderr_style);
        if drain_watch_queue(
            &paths,
            &watch_cursor_key,
            stdout_style,
            &mut printed_watch_message,
        )? {
            last_activity = Instant::now();
        }
    }
    if !warm_profiles.is_empty() {
        start_warm_context_thread(paths.clone(), warm_profiles, *stderr_style);
    }
    let mut ticks = 0_u64;

    loop {
        thread::sleep(Duration::from_millis(poll_ms));
        if !json
            && drain_watch_queue(
                &paths,
                &watch_cursor_key,
                stdout_style,
                &mut printed_watch_message,
            )?
        {
            last_activity = Instant::now();
        }
        let next = FileSnapshot::scan(&root)?;
        let conversation_events = conversations_after(&paths, last_conversation_id)?;
        if snapshot.has_changed(&next) {
            snapshot = settle_snapshot(&root, next, debounce_ms)?;
            last_activity = Instant::now();
            ticks += 1;
            let responses =
                run_tick_for_resolved(&paths, &selected, default_tick_id(), Some(ticks), None)?;
            print_watch_responses(
                &paths,
                &watch_cursor_key,
                responses,
                json,
                stdout_style,
                &mut printed_watch_message,
            )?;
            if max_ticks.is_some_and(|max| ticks >= max) {
                return Ok(());
            }
        }
        if !conversation_events.is_empty() {
            last_activity = Instant::now();
            for event in conversation_events {
                last_conversation_id = event.event_id;
                ticks += 1;
                let target_session_id = if event.session_id.trim().is_empty() {
                    None
                } else {
                    Some(event.session_id)
                };
                let responses = run_tick_for_resolved(
                    &paths,
                    &selected,
                    default_tick_id(),
                    Some(ticks),
                    target_session_id,
                )?;
                print_watch_responses(
                    &paths,
                    &watch_cursor_key,
                    responses,
                    json,
                    stdout_style,
                    &mut printed_watch_message,
                )?;
                if max_ticks.is_some_and(|max| ticks >= max) {
                    return Ok(());
                }
            }
        } else if idle_timeout_ms
            .map(|timeout| last_activity.elapsed() >= Duration::from_millis(timeout))
            .unwrap_or(false)
        {
            return Ok(());
        }
    }
}

fn conversation_watermark(paths: &ProjectPaths) -> Result<u64> {
    StateStore::new(paths.clone()).last_conversation_id()
}

fn conversations_after(paths: &ProjectPaths, event_id: u64) -> Result<Vec<ConversationRecord>> {
    StateStore::new(paths.clone()).conversations_after(event_id)
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
    json: bool,
    stderr_style: &Style,
) -> Result<Vec<Response>> {
    let selected = resolve_selected_profiles(paths, profiles)?;
    ensure_daemon_running(paths, json, stderr_style)?;
    run_tick_for_resolved(paths, &selected, tick_id, None, None)
}

fn run_tick_for_resolved(
    paths: &ProjectPaths,
    profiles: &[ResolvedProfile],
    tick_id: String,
    scheduler_tick: Option<u64>,
    target_session_id: Option<String>,
) -> Result<Vec<Response>> {
    let _ = ensure_watcher_check_ins(paths, profiles)?;
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
                target_session_id: target_session_id.clone(),
            },
        )?);
    }
    Ok(responses)
}

fn ensure_watcher_check_ins(
    paths: &ProjectPaths,
    profiles: &[ResolvedProfile],
) -> Result<Vec<u64>> {
    ensure_watcher_check_ins_for(paths, profiles, None)
}

fn ensure_watcher_check_ins_for(
    paths: &ProjectPaths,
    profiles: &[ResolvedProfile],
    target_session_id: Option<&str>,
) -> Result<Vec<u64>> {
    match ensure_watcher_check_ins_once(paths, profiles, target_session_id) {
        Ok(message_ids) => Ok(message_ids),
        Err(error) if is_daemon_schema_mismatch(&error) => {
            daemon::restart(Some(paths.identity().root()))?;
            ensure_watcher_check_ins_once(paths, profiles, target_session_id)
        }
        Err(error) => Err(error),
    }
}

fn ensure_watcher_check_ins_once(
    paths: &ProjectPaths,
    profiles: &[ResolvedProfile],
    target_session_id: Option<&str>,
) -> Result<Vec<u64>> {
    let mut message_ids = Vec::new();
    for resolved in profiles {
        let response = send_request(
            paths.socket_path(),
            &Request::EnsureWatcherCheckIn {
                protocol: PROTOCOL_VERSION,
                watcher: resolved.profile.name.clone(),
                target_session_id: target_session_id.map(str::to_owned),
            },
        )?;
        match response {
            Response::WatcherCheckIn {
                message_id: Some(message_id),
                ..
            } => message_ids.push(message_id),
            Response::WatcherCheckIn { .. } => {}
            Response::Error { code, message, .. } => {
                return Err(EyesError::Protocol(format!("{code}: {message}")));
            }
            other => {
                return Err(EyesError::Protocol(format!(
                    "unexpected watcher check-in response: {other:?}"
                )));
            }
        }
    }
    Ok(message_ids)
}

fn is_daemon_schema_mismatch(error: &EyesError) -> bool {
    let text = error.to_string();
    matches!(error, EyesError::Json(_))
        || text.contains("unknown variant")
        || text.contains("missing field")
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

fn warm_start_profiles(profiles: &[ResolvedProfile]) -> Result<Vec<ResolvedProfile>> {
    profiles
        .iter()
        .filter_map(|resolved| match profile_warm_start(resolved) {
            Ok(true) => Some(Ok(resolved.clone())),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn profile_warm_start(resolved: &ResolvedProfile) -> Result<bool> {
    profiles::bool_setting(&resolved.profile, "warm_start").map(|enabled| enabled.unwrap_or(false))
}

fn start_warm_context_thread(paths: ProjectPaths, profiles: Vec<ResolvedProfile>, style: Style) {
    thread::spawn(move || {
        let tick_id = format!("warm-{}", now_ms());
        match run_tick_for_resolved(&paths, &profiles, tick_id, None, None) {
            Ok(responses) => {
                for response in responses {
                    if let Response::Error { code, message, .. } = response {
                        eprintln!(
                            "{}",
                            style.line(
                                "eyes",
                                "warm-up",
                                "failed",
                                terminal::details(&[("error", format!("{code}: {message}"))]),
                            )
                        );
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    style.line(
                        "eyes",
                        "warm-up",
                        "failed",
                        terminal::details(&[("error", error.to_string())]),
                    )
                );
            }
        }
    });
}

fn print_tick_responses(responses: Vec<Response>, json: bool, style: &Style) -> Result<()> {
    if json {
        if responses.len() == 1 {
            println!("{}", serde_json::to_string_pretty(&responses[0])?);
        } else {
            println!("{}", serde_json::to_string_pretty(&responses)?);
        }
        return Ok(());
    }

    for response in responses {
        print_tick_response(response, style)?;
    }
    Ok(())
}

fn print_tick_response(response: Response, style: &Style) -> Result<()> {
    match response {
        Response::WatcherRun {
            watcher,
            summary,
            messages,
            statuses,
            ..
        } => {
            print_human_watcher_run(&watcher, &summary, &messages, &statuses, style);
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

fn watch_cursor_key() -> String {
    format!("watch:{}:{}:feed", std::process::id(), now_ms())
}

fn seed_watch_cursor(paths: &ProjectPaths, cursor_key: &str, check_in_ids: &[u64]) -> Result<()> {
    let Some(first_check_in) = check_in_ids.iter().min().copied() else {
        return Ok(());
    };
    let through_message_id = first_check_in.saturating_sub(1);
    commit_hook_cursor_if_possible(
        Some(paths.identity().root()),
        "hook".to_owned(),
        cursor_key.to_owned(),
        through_message_id,
    )
}

fn drain_watch_queue(
    paths: &ProjectPaths,
    cursor_key: &str,
    style: &Style,
    printed_watch_message: &mut bool,
) -> Result<bool> {
    let mut printed_any = false;
    loop {
        let batch = fetch_message_batch(
            Some(paths.identity().root()),
            "hook",
            cursor_key,
            Some(1000),
            None,
            false,
            true,
        )?;
        let Some(last_message_id) = batch.messages.last().map(|message| message.message_id) else {
            return Ok(printed_any);
        };
        for message in &batch.messages {
            print_watch_message_block(
                watch_ipc_message_block(message, style),
                printed_watch_message,
            );
            printed_any = true;
        }
        commit_hook_cursor_if_possible(
            Some(paths.identity().root()),
            "hook".to_owned(),
            cursor_key.to_owned(),
            last_message_id,
        )?;
        if batch.messages.len() < 1000 {
            return Ok(printed_any);
        }
    }
}

fn print_watch_responses(
    paths: &ProjectPaths,
    cursor_key: &str,
    responses: Vec<Response>,
    json: bool,
    style: &Style,
    printed_watch_message: &mut bool,
) -> Result<()> {
    if json {
        return print_tick_responses(responses, json, style);
    }

    for response in &responses {
        if let Response::Error { code, message, .. } = response {
            return Err(EyesError::Protocol(format!("{code}: {message}")));
        }
    }
    drain_watch_queue(paths, cursor_key, style, printed_watch_message)?;
    Ok(())
}

fn print_watch_message_block(block: String, printed: &mut bool) {
    if *printed {
        println!();
    }
    println!("{block}");
    *printed = true;
}

fn watch_ipc_message_block(message: &IpcMessage, style: &Style) -> String {
    let watcher = message
        .payload
        .get("watcher")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("eyes");
    let prefix = format!("{} {} ", style.brand(watcher), style.arrow());
    render_watch_message_block(
        &prefix,
        &watch_ipc_message_text(message),
        ipc_refs_summary(message),
    )
}

fn watch_ipc_message_text(message: &IpcMessage) -> String {
    message
        .payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            serde_json::to_string(&message.payload)
                .unwrap_or_else(|_| "<unrenderable watcher message>".to_owned())
        })
}

fn ipc_refs_summary(message: &IpcMessage) -> Option<String> {
    let refs = message.payload.get("refs")?.as_array()?;
    if refs.is_empty() {
        return None;
    }
    let rendered = refs
        .iter()
        .take(3)
        .filter_map(|reference| {
            let path = reference.get("path")?.as_str()?;
            let line = reference.get("line").and_then(serde_json::Value::as_u64);
            Some(match line {
                Some(line) => format!("{path}:{line}"),
                None => path.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(format!("[{}]", rendered.join(", ")))
    }
}

fn watcher_pulse(watcher: &str, summary: &WatcherRunSummary, style: &Style) -> String {
    let state = if summary.state == "quiet" {
        "quiet"
    } else {
        summary.severity.as_str()
    };
    let text = terminal::compact_text(&summary.text, 140);
    style.line("eyes", watcher, state, text)
}

fn print_human_watcher_run(
    watcher: &str,
    summary: &WatcherRunSummary,
    messages: &[WatcherMessage],
    statuses: &[WatcherStatusEvent],
    style: &Style,
) {
    if messages.is_empty() {
        println!("{}", watcher_pulse(watcher, summary, style));
        return;
    }
    let mut printed = false;
    for message in messages {
        print_watch_message_block(watcher_message_block(message, style), &mut printed);
    }
    for status in statuses.iter().filter(|status| status.severity != "info") {
        if printed {
            println!();
            printed = false;
        }
        println!("{}", watcher_status_line(status, style));
    }
}

fn watcher_message_block(message: &WatcherMessage, style: &Style) -> String {
    let prefix = format!(
        "{} ",
        style.line("eyes", &message.watcher, &message.severity, "")
    );
    let refs = if message.refs.is_empty() {
        None
    } else {
        Some(format_refs(&message.refs))
    };
    render_watch_message_block(&prefix, &message.text, refs)
}

fn render_watch_message_block(prefix: &str, text: &str, refs: Option<String>) -> String {
    let mut rendered = String::new();
    let mut lines = text.lines();
    let first_line = lines.next().unwrap_or("");
    rendered.push_str(prefix);
    rendered.push_str(first_line);

    for line in lines {
        rendered.push('\n');
        rendered.push_str("  ");
        rendered.push_str(line);
    }

    if let Some(refs) = refs {
        rendered.push('\n');
        rendered.push_str("  ");
        rendered.push_str(&refs);
    }

    rendered
}

fn watcher_status_line(status: &WatcherStatusEvent, style: &Style) -> String {
    let details = format!("{}: {}", status.outcome, status.text);
    style.line(
        "eyes",
        &status.watcher,
        &status.severity,
        terminal::compact_text(&details, 180),
    )
}

fn format_refs(refs: &[WatcherRef]) -> String {
    let mut formatted = refs.iter().take(3).map(format_ref).collect::<Vec<_>>();
    if refs.len() > 3 {
        formatted.push(format!("+{} more", refs.len() - 3));
    }
    format!("[{}]", formatted.join(", "))
}

fn format_ref(reference: &WatcherRef) -> String {
    match (reference.line, reference.column) {
        (Some(line), Some(column)) => format!("{}:{line}:{column}", reference.path),
        (Some(line), None) => format!("{}:{line}", reference.path),
        _ => reference.path.clone(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn default_command() -> Command {
    Command::Watch {
        profiles: Vec::new(),
        project: None,
        poll_ms: 250,
        debounce_ms: 250,
        max_ticks: None,
        idle_timeout_ms: None,
        json: false,
    }
}

fn print_watch_started(
    paths: &ProjectPaths,
    selected: &[ResolvedProfile],
    warm_profiles: &[ResolvedProfile],
    quiet: bool,
    style: &Style,
) {
    if quiet {
        return;
    }
    let message_prefix = match selected {
        [single] => single.profile.name.as_str(),
        _ => "<watcher>",
    };
    eprintln!(
        "{} {}  {} {}",
        style.brand("eyes"),
        style.success("watch"),
        style.muted("project"),
        paths.identity().root().display()
    );
    eprintln!(
        "  {} {}  {}  {} file, conversation",
        style.muted(profile_label(selected)),
        profile_list(selected, style),
        style.muted("|"),
        style.muted("triggers")
    );
    if !warm_profiles.is_empty() {
        eprintln!(
            "  {} {} background repo context",
            style.muted("warm-up"),
            profile_names(warm_profiles, style)
        );
    }
    eprintln!(
        "  {} {} {} <watcher message>",
        style.muted("messages"),
        style.brand(message_prefix),
        style.arrow()
    );
}

fn profile_label(profiles: &[ResolvedProfile]) -> &'static str {
    if profiles.len() == 1 {
        "watcher profile"
    } else {
        "watcher profiles"
    }
}

fn profile_list(profiles: &[ResolvedProfile], style: &Style) -> String {
    profiles
        .iter()
        .map(|resolved| profile_entry(resolved, style))
        .collect::<Vec<_>>()
        .join(", ")
}

fn profile_entry(resolved: &ResolvedProfile, style: &Style) -> String {
    let mut badges = vec![profile_source_label(resolved)];
    if resolved.profile.default {
        badges.push("default profile");
    }
    format!(
        "{} {}",
        style.brand(&resolved.profile.name),
        style.muted(&format!("[{}]", badges.join(", ")))
    )
}

fn profile_names(profiles: &[ResolvedProfile], style: &Style) -> String {
    profiles
        .iter()
        .map(|resolved| style.brand(&resolved.profile.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn profile_source_label(resolved: &ResolvedProfile) -> &'static str {
    match resolved.source {
        profiles::ProfileSource::Project => "project",
        profiles::ProfileSource::User => "user",
        profiles::ProfileSource::Bundled => "bundled",
    }
}

fn ensure_daemon_running(paths: &ProjectPaths, quiet: bool, style: &Style) -> Result<()> {
    match daemon::status(Some(paths.identity().root())) {
        Ok(Response::Status { .. }) => Ok(()),
        Ok(Response::Error { code, message, .. }) => Err(EyesError::Protocol(format!(
            "daemon returned {code}: {message}"
        ))),
        Ok(other) => Err(EyesError::Protocol(format!(
            "unexpected daemon status response: {other:?}"
        ))),
        Err(EyesError::NotRunning) | Err(EyesError::Io(_)) => {
            match daemon::start_detached(Some(paths.identity().root())) {
                Ok(started) => {
                    if !quiet {
                        eprintln!(
                            "{}",
                            style.line(
                                "eyes",
                                "daemon",
                                "started",
                                terminal::details(&[
                                    ("pid", started.pid.to_string()),
                                    ("project", started.project_root),
                                    ("log", started.log_path.display().to_string()),
                                ]),
                            )
                        );
                    }
                    Ok(())
                }
                Err(EyesError::AlreadyRunning) => Ok(()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
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
