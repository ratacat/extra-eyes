use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use extra_eyes::daemon;
use extra_eyes::ipc::Response;
use extra_eyes::{EyesError, Result};

#[derive(Debug, Parser)]
#[command(name = "eyesd", about = "Extra Eyes daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Start the daemon for a project")]
    Start {
        #[arg(
            long,
            help = "Run in the foreground instead of spawning a background daemon"
        )]
        foreground: bool,
        #[arg(long, help = "Project root to watch; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Show daemon status for a project")]
    Status {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Ask the daemon for a project to stop")]
    Stop {
        #[arg(long, help = "Project root; defaults to the current project")]
        project: Option<PathBuf>,
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
        Command::Start {
            foreground,
            project,
            json,
        } => {
            if foreground {
                if json {
                    return Err(EyesError::Config(
                        "--json is only supported for detached start".to_owned(),
                    ));
                }
                daemon::start_foreground(project.as_deref())
            } else {
                let started = daemon::start_detached(project.as_deref())?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&started)?);
                } else {
                    println!(
                        "started pid={} project={} log={}",
                        started.pid,
                        started.project_root,
                        started.log_path.display()
                    );
                }
                Ok(())
            }
        }
        Command::Status { project, json } => {
            let response = daemon::status(project.as_deref())?;
            print_status(response, json)
        }
        Command::Stop { project } => {
            let response = daemon::stop(project.as_deref())?;
            match response {
                Response::Stopping { .. } => {
                    println!("stopping");
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
    }
}

fn print_status(response: Response, json: bool) -> Result<()> {
    match response {
        Response::Status {
            pid,
            project_root,
            project_hash,
            socket_path,
            state_dir,
            ..
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "running",
                        "pid": pid,
                        "project_root": project_root,
                        "project_hash": project_hash,
                        "socket_path": socket_path,
                        "state_dir": state_dir,
                    }))?
                );
            } else {
                println!("running pid={pid} project={project_root}");
            }
            Ok(())
        }
        Response::Error { code, message, .. } => Err(EyesError::Protocol(format!(
            "daemon returned {code}: {message}"
        ))),
        other => Err(EyesError::Protocol(format!(
            "unexpected status response: {other:?}"
        ))),
    }
}
