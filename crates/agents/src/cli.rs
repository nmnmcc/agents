mod mail;

use crate::{
    config::{self, schema},
    daemon, harness, mail as mailbox, service, types,
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    io,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "With no command, watch the current project and run agents for unread mail."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    #[command(hide = true)]
    MailMcp { socket: PathBuf },
    /// Run a project agent with the task read from stdin.
    Run {
        directory: Option<PathBuf>,
        /// Select a project agent instead of using the project's default.
        #[arg(long)]
        agent: Option<types::Id>,
    },
    /// Access agent mailboxes, send mail, or inspect saved messages.
    Mail {
        /// Find the project from this directory (default: current directory).
        #[arg(long, global = true)]
        directory: Option<PathBuf>,
        #[command(subcommand)]
        command: mail::Action,
    },
    /// Validate or preview a project's harness configuration.
    Config {
        #[command(subcommand)]
        command: ConfigAction,
    },
    /// Print or install a user service for the current project.
    #[command(args_conflicts_with_subcommands = true)]
    Service {
        /// Definition format to print (defaults to the current OS).
        #[arg(long, value_enum)]
        platform: Option<service::Platform>,
        #[command(subcommand)]
        command: Option<ServiceAction>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install, enable and start the service for the current user.
    Install,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Load a project, resolve its references and prepare its harness configuration.
    Check { directory: Option<PathBuf> },
    /// Show the command and generated files, without writing them or running the harness.
    Render { directory: Option<PathBuf> },
    /// Print the shared JSON Schema without loading any configuration.
    Schema,
}

fn run(args: Args) -> Result<ExitCode> {
    let Some(command) = args.command else {
        daemon::serve()?;
        return Ok(ExitCode::SUCCESS);
    };
    match command {
        Action::Service { platform, command } => {
            let workspace = config::Workspace::load(None)?;
            let platform = platform.map_or_else(service::Platform::native, Ok)?;
            let definition = service::Definition::new(
                platform,
                workspace.root(),
                &std::env::current_exe().context("locate agents executable")?,
            )?;
            if command.is_some() {
                println!("{}", definition.install()?.display());
            } else {
                use io::Write;
                io::stdout().lock().write_all(definition.text.as_bytes())?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::MailMcp { socket } => {
            mailbox::mcp::serve(socket)?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Run { directory, agent } => {
            let workspace = config::Workspace::load(directory.as_deref())?;
            let (project, routes) = workspace.project_for_run(agent.as_ref())?;
            let identity = routes
                .as_ref()
                .and_then(|routes| routes.identity(&project).map(|agent| (routes, agent)));
            mailbox::execute(
                project,
                io::stdin().lock(),
                workspace.mail(),
                identity,
                None,
            )
        }
        Action::Config { command } => {
            command.run()?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Mail { directory, command } => command.run(directory),
    }
}

impl ConfigAction {
    fn run(self) -> Result<()> {
        fn load(directory: Option<&Path>) -> Result<(config::Project, harness::Plan)> {
            let project = config::Workspace::load(directory)?.project(None)?;
            let plan = harness::Plan::render(&project, "<run-dir>".as_ref())?;
            Ok((project, plan))
        }

        match self {
            Self::Check { directory } => {
                load(directory.as_deref())?;
                Ok(())
            }
            Self::Render { directory } => {
                let (project, plan) = load(directory.as_deref())?;
                if project.references.used {
                    print_json(&serde_json::json!({
                        "cwd": project.root,
                        "redacted": true,
                        "reason": "project configuration uses external value references"
                    }))
                } else {
                    print_json(&plan)
                }
            }
            Self::Schema => print_json(&schema::document()),
        }
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    use io::Write;
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

pub(crate) fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agents: {error:#}");
            ExitCode::FAILURE
        }
    }
}
