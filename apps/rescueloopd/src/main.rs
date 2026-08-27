use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rescueloop_agent::{ALLOWED_ACTIONS, HttpAnalysisProvider};
use rescueloop_core::{AnalysisProvider, AnalysisRequest, Incident};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tracing::info;

mod console;
mod incident_store;
mod repair_flow;
mod service;
mod tui;

pub(crate) use console::configured_provider;
use console::{console, index_command, load_settings, setup, sources};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(cli.incident_dir, None, None).await,
        Some(Command::Watch) => watch(&cli.incident_dir).await,
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

async fn watch(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).await?;
    let settings = load_settings(dir).await?;
    let sources = rescueloop_platform::event_sources(&settings.enabled_sources)?;
    let source_names: Vec<_> = sources.iter().map(|source| source.name()).collect();
    println!("RescueLoop {}", env!("CARGO_PKG_VERSION"));
    println!("Status: READY — monitoring for objective failures");
    println!(
        "Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Event sources: {}", source_names.join(", "));
    println!("Incidents: {}", dir.display());
    println!("Privacy: local detection only; AI analysis starts only on request");
    println!("Waiting for a new failure event...\n");
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    for mut source in sources {
        let sender = sender.clone();
        tokio::spawn(async move {
            info!(source = source.name(), "event source started");
            let mut retry_delay = Duration::from_secs(2);
            loop {
                match source.next_incident().await {
                    Ok(incident) => {
                        retry_delay = Duration::from_secs(2);
                        if sender.send(incident).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(source = source.name(), %error, "event source reconnecting");
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
        });
    }
    drop(sender);
    while let Some(incident) = events.recv().await {
        let (destination, created) = save_incident(dir, &incident).await?;
        if !created {
            continue;
        }
        println!("DETECTED: {:?}: {}", incident.kind, incident.message);
        println!("Incident saved to {}", destination.display());
        println!(
            "Analysis has NOT started. Run: rescueloop analyze '{}' --endpoint <URL>",
            destination.display()
        );
    }
    anyhow::bail!("all event sources stopped")
}

async fn run_supervised(
    dir: &Path,
    executable: PathBuf,
    args: Vec<String>,
    record_args: bool,
) -> Result<()> {
    match rescueloop_platform::supervise(&executable, &args, record_args).await? {
        None => println!("PASSED: original action exited successfully; no incident created."),
        Some(incident) => {
            let (destination, _) = save_incident(dir, &incident).await?;
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
    let request = AnalysisRequest::bounded(incident, allowed_actions);
    let response = provider.analyze(&request).await?;
    if let Some(output) = output {
        fs::write(output, serde_json::to_vec_pretty(&response)?).await?;
    }
    Ok(response)
}
