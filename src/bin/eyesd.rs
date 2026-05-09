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
    Start {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
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
        } => {
            if !foreground {
                return Err(EyesError::Config(
                    "detached daemon mode is not implemented yet; use --foreground".to_owned(),
                ));
            }
            daemon::start_foreground(project.as_deref())
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
