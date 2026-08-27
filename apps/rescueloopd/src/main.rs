use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rescueloop_agent::{ALLOWED_ACTIONS, HttpAnalysisProvider};
use rescueloop_core::{AnalysisProvider, AnalysisRequest, Incident};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{error, info};

mod console;
mod incident_store;
mod logging;
mod mcp;
mod repair_flow;
mod service;
mod storage;
mod tui;
mod watch_health;
mod watcher;

pub(crate) use console::configured_provider;
use console::{console, index_command, setup, sources};
pub(crate) use incident_store::local_timestamp;
pub(crate) use incident_store::{dismiss_incident, record_incident_status};
use incident_store::{incidents, save_incident};
pub(crate) use repair_flow::{repair, repair_silent};

#[derive(Parser)]
#[command(
    name = "rescueloop",
    about = "Detect failures first; analyze only with explicit user intent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, default_value = ".rescueloop/incidents", global = true)]
    incident_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Serve redacted, read-only incident tools over local MCP stdio.
    Mcp,
    /// Monitor OS diagnostic artifacts and persist normalized incidents.
    Watch,
    /// Install, remove, or inspect the per-user background watcher.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Detect installed AI agents and save the selected provider.
    Setup,
    /// Inspect or change enabled event sources.
    Sources {
        #[command(subcommand)]
        action: SourcesAction,
    },
    /// Inspect or safely rebuild the disposable incident index.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Show recent structured operational events.
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        event: Option<String>,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        verify: bool,
        #[arg(long, value_enum, default_value_t = LogOutput::Pretty)]
        output: LogOutput,
    },
    /// Connect to the background detector through the local incident store.
    Console {
        #[arg(long, env = "RESCUELOOP_AI_ENDPOINT")]
        endpoint: Option<String>,
        #[arg(long, env = "RESCUELOOP_AI_TOKEN")]
        token: Option<String>,
        /// Use the line-oriented accessibility/SSH fallback.
        #[arg(long)]
        plain: bool,
    },
    /// Send one saved incident to a user-selected compatible AI endpoint.
    Analyze {
        incident: PathBuf,
        #[arg(long, env = "RESCUELOOP_AI_ENDPOINT")]
        endpoint: String,
        #[arg(long, env = "RESCUELOOP_AI_TOKEN")]
        token: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Run an action under observation and save an incident on non-success exit.
    Run {
        #[arg(long)]
        record_args: bool,
        executable: PathBuf,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Repeat the exact recorded action and report whether it now succeeds.
    Replay { incident: PathBuf },
    /// Dry-run or explicitly apply one proposed repair, then replay.
    Repair {
        incident: PathBuf,
        analysis: PathBuf,
        #[arg(long, default_value_t = 0)]
        action_index: usize,
        #[arg(long)]
        allow_root: Vec<PathBuf>,
        #[arg(long)]
        approve: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    InstallSystem,
    Uninstall,
    UninstallSystem,
    Status,
}

#[derive(Subcommand)]
enum SourcesAction {
    List,
    Enable { name: String },
    Disable { name: String },
}

#[derive(Subcommand)]
enum IndexAction {
    Status,
    Rebuild,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogOutput {
    Pretty,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    storage::prepare_state_store(&cli.incident_dir)?;
    let _log_guard = logging::init(&cli.incident_dir)?;
    let command = cli.command.as_ref().map_or("tui", Command::name);
    info!(
        event = "runtime.started",
        version = env!("CARGO_PKG_VERSION"),
        command,
        pid = std::process::id(),
        "RescueLoop started"
    );
    logging::trigger_test_panic_if_requested();
    let result = run(cli).await;
    match &result {
        Ok(()) => info!(
            event = "runtime.stopped",
            command,
            log_write_errors = _log_guard.write_errors(),
            log_export_drops = _log_guard.export_drops(),
            "RescueLoop stopped"
        ),
        Err(error) => error!(
            event = "runtime.failed",
            command,
            error = %format!("{error:#}"),
            "RescueLoop failed"
        ),
    }
    result
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => tui::run(cli.incident_dir, None, None).await,
        Some(Command::Mcp) => mcp::serve(&cli.incident_dir).await,
        Some(Command::Watch) => watcher::run(&cli.incident_dir).await,
        Some(Command::Service { action }) => match action {
            ServiceAction::Install => service::install(&cli.incident_dir).await,
            ServiceAction::InstallSystem => service::install_system(&cli.incident_dir).await,
            ServiceAction::Uninstall => service::uninstall().await,
            ServiceAction::UninstallSystem => service::uninstall_system().await,
            ServiceAction::Status => service::status().await,
        },
        Some(Command::Setup) => setup(&cli.incident_dir).await,
        Some(Command::Sources { action }) => sources(&cli.incident_dir, action).await,
        Some(Command::Index { action }) => index_command(&cli.incident_dir, action).await,
        Some(Command::Logs {
            lines,
            follow,
            level,
            event,
            correlation_id,
            since,
            until,
            verify,
            output,
        }) => {
            logging::query(
                &cli.incident_dir,
                logging::LogQuery {
                    lines,
                    follow,
                    level,
                    event,
                    correlation_id,
                    since,
                    until,
                    verify,
                    output: match output {
                        LogOutput::Pretty => logging::LogOutput::Pretty,
                        LogOutput::Json => logging::LogOutput::Json,
                    },
                },
            )
            .await
        }
        Some(Command::Console {
            endpoint,
            token,
            plain,
        }) => {
            if plain {
                console(&cli.incident_dir, endpoint, token).await
            } else {
                tui::run(cli.incident_dir, endpoint, token).await
            }
        }
        Some(Command::Analyze {
            incident,
            endpoint,
            token,
            output,
        }) => analyze(&incident, endpoint, token, output.as_deref()).await,
        Some(Command::Run {
            record_args,
            executable,
            args,
        }) => run_supervised(&cli.incident_dir, executable, args, record_args).await,
        Some(Command::Replay { incident }) => replay(&incident).await,
        Some(Command::Repair {
            incident,
            analysis,
            action_index,
            allow_root,
            approve,
        }) => {
            repair(
                &cli.incident_dir,
                &incident,
                &analysis,
                action_index,
                allow_root,
                approve,
            )
            .await
        }
    }
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Watch => "watch",
            Self::Service { .. } => "service",
            Self::Setup => "setup",
            Self::Sources { .. } => "sources",
            Self::Index { .. } => "index",
            Self::Logs { .. } => "logs",
            Self::Console { .. } => "console",
            Self::Analyze { .. } => "analyze",
            Self::Run { .. } => "run",
            Self::Replay { .. } => "replay",
            Self::Repair { .. } => "repair",
        }
    }
}

async fn run_supervised(
    dir: &Path,
    executable: PathBuf,
    args: Vec<String>,
    record_args: bool,
) -> Result<()> {
    match rescueloop_platform::supervise(&executable, &args, record_args).await? {
        None => {
            info!(event = "supervision.passed", "Supervised action succeeded");
            println!("PASSED: original action exited successfully; no incident created.")
        }
        Some(incident) => {
            let (destination, _) = save_incident(dir, &incident).await?;
            info!(event = "supervision.failed", incident_id = %incident.id, kind = ?incident.kind, "Supervised action produced an incident");
            println!("DETECTED: {:?}: {}", incident.kind, incident.message);
            println!("Incident saved to {}", destination.display());
            if record_args {
                println!("Exact replay is available for this incident.");
            } else {
                println!(
                    "Arguments were not stored. Use --record-args only when they contain no secrets and exact replay is needed."
                );
            }
        }
    }
    Ok(())
}

async fn replay(path: &Path) -> Result<()> {
    let incident: Incident =
        serde_json::from_slice(&fs::read(path).await.context("cannot read incident")?)
            .context("invalid incident JSON")?;
    let context = incident
        .launch_context
        .context("incident has no launch context")?;
    let result = rescueloop_platform::verify_replay(&context).await?;
    info!(
        event = "verification.completed",
        incident_id = %incident.id,
        passed = result.passed,
        exit_code = ?result.exit_code,
        duration_ms = result.duration_ms,
        "Replay verification completed"
    );
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result.passed {
        println!("VERIFIED: the exact recorded action now succeeds.");
    } else {
        println!("NOT FIXED: replay still returns a non-success status.");
    }
    Ok(())
}

async fn analyze(
    path: &Path,
    endpoint: String,
    token: Option<String>,
    output: Option<&Path>,
) -> Result<()> {
    let provider = HttpAnalysisProvider::new(endpoint, token);
    let response = analyze_with_provider(path, &provider, output).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if let Some(output) = output {
        println!("Validated analysis saved to {}", output.display());
    }
    println!("No repair was executed. Review the proposal and approve a typed repair separately.");
    Ok(())
}

pub(crate) async fn analyze_with_provider(
    path: &Path,
    provider: &dyn AnalysisProvider,
    output: Option<&Path>,
) -> Result<rescueloop_core::AnalysisResponse> {
    let incident: Incident =
        serde_json::from_slice(&fs::read(path).await.context("cannot read incident")?)
            .context("invalid incident JSON")?;
    let allowed_actions = ALLOWED_ACTIONS
        .iter()
        .copied()
        .filter(|action| cfg!(unix) || *action != "set_permission")
        .map(str::to_string)
        .collect();
    info!(
        event = "analysis.started",
        incident_id = %incident.id,
        provider = provider.name(),
        "Analysis started"
    );
    let incident_id = incident.id;
    let request = AnalysisRequest::bounded(incident, allowed_actions);
    let response = match provider.analyze(&request).await {
        Ok(response) => response,
        Err(error) => {
            error!(
                event = "analysis.failed",
                incident_id = %incident_id,
                provider = provider.name(),
                error = %error,
                "Analysis failed"
            );
            return Err(error.into());
        }
    };
    if let Some(output) = output {
        fs::write(output, serde_json::to_vec_pretty(&response)?).await?;
    }
    info!(
        event = "analysis.completed",
        incident_id = %incident_id,
        provider = provider.name(),
        proposed_actions = response.proposed_actions.len(),
        needs_more_evidence = response.needs_more_evidence,
        "Analysis completed"
    );
    Ok(response)
}
