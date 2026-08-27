use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rescueloop_agent::{ALLOWED_ACTIONS, AgentConfig, CliAnalysisProvider, HttpAnalysisProvider};
use rescueloop_core::{AnalysisProvider, AnalysisRequest, Incident};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tracing::info;

mod service;
mod tui;

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

const SOURCE_NAMES: &[&str] = &["system-artifacts", "containers", "os-log"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "default_sources")]
    enabled_sources: Vec<String>,
}

fn default_sources() -> Vec<String> {
    SOURCE_NAMES
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled_sources: default_sources(),
        }
    }
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

async fn console(dir: &Path, endpoint: Option<String>, token: Option<String>) -> Result<()> {
    let explicit_endpoint = endpoint.is_some();
    let mut provider = configured_provider(dir, endpoint, token.clone()).await?;
    if provider.is_none() && !explicit_endpoint {
        println!("No AI agent is configured yet.");
        if confirm("Run first-time agent setup now? [y/N] ")? {
            setup(dir).await?;
            provider = configured_provider(dir, None, token).await?;
        }
    }
    println!("RescueLoop Console {}", env!("CARGO_PKG_VERSION"));
    println!("Connected to local incident store: {}", dir.display());
    println!(
        "AI provider: {}",
        provider
            .as_ref()
            .map(|value| value.name())
            .unwrap_or("not configured")
    );
    println!("Type 'help' for commands.\n");
    print_incidents(dir).await?;
    println!("Enter an incident number to open it. Example: 1\n");

    let known: HashSet<_> = incidents(dir)
        .await?
        .into_iter()
        .map(|(incident, _)| incident.id)
        .collect();
    let watch_dir = dir.to_path_buf();
    let live_updates = tokio::spawn(async move {
        let mut known = known;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Ok(values) = incidents(&watch_dir).await else {
                continue;
            };
            let mut new_values: Vec<_> = values
                .into_iter()
                .filter(|(incident, _)| !known.contains(&incident.id))
                .collect();
            new_values.reverse();
            for (incident, _) in new_values {
                known.insert(incident.id);
                println!(
                    "\nNEW INCIDENT: {} — {:?} — {:?} — {}\nUse 'incidents' to refresh numbering or 'details 1' for the newest incident.",
                    incident
                        .application
                        .as_deref()
                        .unwrap_or("unknown application"),
                    incident.kind,
                    incident.status,
                    local_timestamp(incident.observed_at)
                );
                print!("rescueloop> ");
                let _ = io::stdout().flush();
            }
        }
    });

    loop {
        print!("rescueloop> ");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let parts: Vec<&str> = input.split_whitespace().collect();
        match parts.as_slice() {
            [] => {}
            ["help"] => print_console_help(),
            ["incidents"] | ["list"] => print_incidents(dir).await?,
            ["details", number] => {
                let incident = incident_by_number(dir, number).await?;
                println!("{}", serde_json::to_string_pretty(&incident)?);
            }
            ["replay", number] => {
                let (_, path) = incident_and_path_by_number(dir, number).await?;
                replay(&path).await?;
            }
            ["analyze", number] => {
                incident_menu(dir, number, provider.as_deref()).await?;
            }
            [number] if number.parse::<usize>().is_ok() => {
                incident_menu(dir, number, provider.as_deref()).await?;
            }
            ["quit"] | ["exit"] => break,
            [command, ..] => println!("Unknown or incomplete command: {command}. Type 'help'."),
        }
    }
    live_updates.abort();
    println!("Console disconnected. The background watcher keeps running.");
    Ok(())
}

async fn incident_menu(
    dir: &Path,
    number: &str,
    provider: Option<&dyn AnalysisProvider>,
) -> Result<()> {
    loop {
        let (incident, path) = incident_and_path_by_number(dir, number).await?;
        println!(
            "\nSelected: {} — {:?} — {}",
            incident
                .application
                .as_deref()
                .unwrap_or("unknown application"),
            incident.kind,
            local_timestamp(incident.observed_at)
        );
        println!("[1] Analyze with AI");
        println!("[2] View technical details");
        println!("[3] Replay original action");
        println!("[0] Back to incidents");
        print!("Choose an action: ");
        io::stdout().flush()?;
        let mut choice = String::new();
        io::stdin().read_line(&mut choice)?;
        match choice.trim() {
            "0" => return Ok(()),
            "1" => {
                let Some(provider) = provider else {
                    println!("No AI agent is configured. Run setup from the main console.");
                    continue;
                };
                println!("AI agent: {}", provider.name());
                if !confirm("Send scrubbed technical evidence for analysis? [y/N] ")? {
                    println!("Analysis cancelled.");
                    continue;
                }
                let analysis_dir = dir.parent().unwrap_or(dir).join("analyses");
                fs::create_dir_all(&analysis_dir).await?;
                let output = analysis_dir.join(format!("{}.json", incident.id));
                let analysis = analyze_with_provider(&path, provider, Some(&output)).await?;
                println!("\nAI DIAGNOSIS\n{}", analysis.summary);
                if analysis.proposed_actions.is_empty() {
                    if analysis.needs_more_evidence {
                        println!("\nNO SAFE FIX PROPOSED — more evidence is required.");
                    } else {
                        println!("\nNO APPLICABLE REPAIR FOUND.");
                    }
                    println!("Nothing was changed on your computer.");
                    continue;
                }
                let proposal = &analysis.proposed_actions[0];
                println!("\nProposed repair: {}", proposal.action_type);
                println!("Reason: {}", proposal.reason);
                println!(
                    "Parameters: {}",
                    serde_json::to_string_pretty(&proposal.parameters)?
                );
                let target = proposal
                    .parameters
                    .get("target")
                    .and_then(|value| value.as_str())
                    .map(PathBuf::from);
                let allowed_roots = target
                    .as_ref()
                    .and_then(|target| target.parent())
                    .map(PathBuf::from)
                    .into_iter()
                    .collect::<Vec<_>>();
                println!("\nSafety review (no changes yet):");
                repair(dir, &path, &output, 0, allowed_roots.clone(), false).await?;
                if confirm("Apply this exact repair and replay the original action? [y/N] ")? {
                    repair(dir, &path, &output, 0, allowed_roots, true).await?;
                    return Ok(());
                }
                println!("Repair cancelled; no changes made.");
            }
            "2" => println!("{}", serde_json::to_string_pretty(&incident)?),
            "3" => replay(&path).await?,
            _ => println!("Choose 0, 1, 2, or 3."),
        }
    }
}

async fn setup(incident_dir: &Path) -> Result<()> {
    let detected = rescueloop_agent::detect_cli_agents();
    println!("RescueLoop setup\n");
    if detected.is_empty() {
        println!("No supported local AI agents found in PATH.");
        println!("You can still use an HTTP adapter with --endpoint <URL>.");
    } else {
        println!("Detected AI agents:");
        for (index, agent) in detected.iter().enumerate() {
            println!(
                "[{}] {:?} — {}",
                index + 1,
                agent.agent,
                agent.executable.display()
            );
        }
        let config = loop {
            print!(
                "Select exactly one agent [1-{}], or q to skip AI setup: ",
                detected.len()
            );
            io::stdout().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            let answer = answer.trim();
            if answer.eq_ignore_ascii_case("q") {
                println!("AI setup skipped; detection setup will continue.");
                break None;
            }
            let Ok(selected) = answer.parse::<usize>() else {
                println!("A numeric selection is required; Enter alone does not select a default.");
                continue;
            };
            let Some(config) = selected
                .checked_sub(1)
                .and_then(|index| detected.get(index))
            else {
                println!("Selection is out of range.");
                continue;
            };
            break Some(config);
        };
        if let Some(config) = config {
            let path = config_path(incident_dir);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&path, serde_json::to_vec_pretty(config)?).await?;
            println!("Selected: {:?}", config.agent);
            println!("Configuration saved to {}", path.display());
            println!("The agent runs read-only; Repair IR is validated before approval.");
        }
    }

    let mut settings = load_settings(incident_dir).await?;
    println!("\nEvent sources:");
    for source in SOURCE_NAMES {
        let enabled = settings.enabled_sources.iter().any(|value| value == source);
        if confirm_default(
            &format!(
                "Enable {source}? [{}] ",
                if enabled { "Y/n" } else { "y/N" }
            ),
            enabled,
        )? {
            if !enabled {
                settings.enabled_sources.push((*source).into());
            }
        } else if enabled {
            settings.enabled_sources.retain(|value| value != source);
        }
    }
    save_settings(incident_dir, &settings).await?;

    let installed = if confirm_default("\nInstall `rescueloop` into your user PATH? [Y/n] ", true)?
    {
        let destination = service::install_to_path().await?;
        println!("Installed executable: {}", destination.display());
        Some(destination)
    } else {
        None
    };
    if confirm_default(
        "Start RescueLoop automatically when you sign in? [Y/n] ",
        true,
    )? {
        service::install_using(incident_dir, installed.as_deref()).await?;
    }
    println!("\nSetup complete. Run `rescueloop` to open the console.");
    Ok(())
}

fn settings_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("settings.json")
}

async fn load_settings(incident_dir: &Path) -> Result<Settings> {
    let path = settings_path(incident_dir);
    if !fs::try_exists(&path).await? {
        return Ok(Settings::default());
    }
    serde_json::from_slice(&fs::read(&path).await?).context("invalid RescueLoop settings")
}

async fn save_settings(incident_dir: &Path, settings: &Settings) -> Result<()> {
    let path = settings_path(incident_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(settings)?).await?;
    Ok(())
}

async fn sources(incident_dir: &Path, action: SourcesAction) -> Result<()> {
    let mut settings = load_settings(incident_dir).await?;
    let mut changed = false;
    match action {
        SourcesAction::List => {}
        SourcesAction::Enable { name } | SourcesAction::Disable { name }
            if !SOURCE_NAMES.contains(&name.as_str()) =>
        {
            anyhow::bail!(
                "unknown event source `{name}`; valid sources: {}",
                SOURCE_NAMES.join(", ")
            )
        }
        SourcesAction::Enable { name } => {
            if !settings.enabled_sources.contains(&name) {
                settings.enabled_sources.push(name);
                save_settings(incident_dir, &settings).await?;
                changed = true;
            }
        }
        SourcesAction::Disable { name } => {
            settings.enabled_sources.retain(|value| value != &name);
            save_settings(incident_dir, &settings).await?;
            changed = true;
        }
    }
    for source in SOURCE_NAMES {
        println!(
            "{:<18} {}",
            source,
            if settings.enabled_sources.iter().any(|value| value == source) {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    if changed {
        if service::restart_if_installed().await? {
            println!("Background watcher restarted with the new source configuration.");
        } else {
            println!("Settings saved. They apply on the next watcher start.");
        }
    }
    Ok(())
}

fn config_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("config.json")
}

pub(crate) async fn configured_provider(
    incident_dir: &Path,
    endpoint: Option<String>,
    token: Option<String>,
) -> Result<Option<Box<dyn AnalysisProvider>>> {
    if let Some(endpoint) = endpoint {
        return Ok(Some(Box::new(HttpAnalysisProvider::new(endpoint, token))));
    }
    let path = config_path(incident_dir);
    if !fs::try_exists(&path).await? {
        return Ok(None);
    }
    let config: AgentConfig = serde_json::from_slice(&fs::read(path).await?)
        .context("invalid RescueLoop agent config")?;
    Ok(Some(Box::new(CliAnalysisProvider::new(config))))
}

fn print_console_help() {
    println!("<number>        Open a guided incident menu (recommended)");
    println!("incidents       List newest incidents");
    println!("details <n>     Show local evidence for an incident");
    println!("analyze <n>     Ask the configured AI provider to analyze it (with consent)");
    println!("replay <n>      Repeat an exact recorded action when available");
    println!("quit            Disconnect; watcher continues in background");
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn confirm_default(prompt: &str, default: bool) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

pub(crate) async fn incidents(dir: &Path) -> Result<Vec<(Incident, PathBuf)>> {
    let mut result = Vec::new();
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return Ok(result);
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = fs::read(&path).await
            && let Ok(incident) = serde_json::from_slice::<Incident>(&bytes)
        {
            result.push((incident, path));
        }
    }
    // Incident JSON is immutable evidence; lifecycle changes live in the append-only ledger.
    // Reconcile the latest status here so every UI sees the current state.
    if let Ok(entries) = rescueloop_ledger::load(&ledger_path(dir)).await {
        let latest: std::collections::HashMap<_, _> = entries
            .into_iter()
            .map(|entry| (entry.incident_id, entry.status))
            .collect();
        for (incident, _) in &mut result {
            if let Some(status) = latest.get(&incident.id) {
                incident.status = status.clone();
            }
        }
    }
    result.retain(|(incident, _)| {
        let from_system_watcher = incident.evidence.iter().any(|evidence| {
            matches!(
                evidence.source.as_str(),
                "macos-diagnostic-reports" | "windows-error-reporting"
            )
        });
        let is_self = incident
            .application
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("rescueloop"));
        !(from_system_watcher && is_self)
    });
    result.sort_by_key(|item| std::cmp::Reverse(item.0.observed_at));
    Ok(result)
}

async fn print_incidents(dir: &Path) -> Result<()> {
    let values = incidents(dir).await?;
    if values.is_empty() {
        println!("No incidents detected yet.");
        return Ok(());
    }
    println!("{} incident(s):", values.len());
    for (index, (incident, _)) in values.iter().enumerate() {
        println!(
            "[{}] {} — {:?} — {:?} — {}",
            index + 1,
            incident
                .application
                .as_deref()
                .unwrap_or("unknown application"),
            incident.kind,
            incident.status,
            local_timestamp(incident.observed_at)
        );
    }
    Ok(())
}

pub(crate) fn local_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

async fn incident_and_path_by_number(dir: &Path, number: &str) -> Result<(Incident, PathBuf)> {
    let index: usize = number
        .parse()
        .context("incident number must be a positive integer")?;
    if index == 0 {
        anyhow::bail!("incident numbering starts at 1")
    }
    incidents(dir)
        .await?
        .into_iter()
        .nth(index - 1)
        .context("incident number is out of range")
}

async fn incident_by_number(dir: &Path, number: &str) -> Result<Incident> {
    Ok(incident_and_path_by_number(dir, number).await?.0)
}

async fn save_incident(dir: &Path, incident: &Incident) -> Result<(PathBuf, bool)> {
    fs::create_dir_all(dir).await?;
    let group_key = incident_group_key(incident);
    if let Some((mut existing, path)) = incidents(dir).await?.into_iter().find(|(candidate, _)| {
        (candidate.group_key == group_key || incident_group_key(candidate) == group_key)
            && !matches!(
                candidate.status,
                rescueloop_core::IncidentStatus::VerifiedFixed
                    | rescueloop_core::IncidentStatus::Superseded
            )
    }) {
        existing.group_key = group_key;
        existing.occurrence_count = existing.occurrence_count.max(1) + 1;
        existing.first_observed_at = existing.first_observed_at.or(Some(existing.observed_at));
        existing.last_observed_at = Some(incident.observed_at);
        existing.message = incident.message.clone();
        existing.kind = incident.kind.clone();
        existing.normalized_failure = incident.normalized_failure.clone();
        existing.evidence.extend(incident.evidence.clone());
        if existing.evidence.len() > 20 {
            existing.evidence.drain(..existing.evidence.len() - 20);
        }
        fs::write(&path, serde_json::to_vec_pretty(&existing)?).await?;
        return Ok((path, false));
    }
    let mut incident = incident.clone();
    incident.group_key = group_key;
    incident.occurrence_count = 1;
    incident.first_observed_at = Some(incident.observed_at);
    incident.last_observed_at = Some(incident.observed_at);
    let destination = dir.join(format!("{}.json", incident.id));
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Ok((destination, false));
        }
        Err(error) => return Err(error.into()),
    };
    use tokio::io::AsyncWriteExt;
    file.write_all(&serde_json::to_vec_pretty(&incident)?)
        .await?;
    let entry = rescueloop_ledger::append(
        &ledger_path(dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: None,
            verifier: None,
            status: incident.status.clone(),
            relation_override: None,
        },
    )
    .await?;
    println!("LINEAGE: {:?}", entry.relation);
    Ok((destination, true))
}

fn incident_group_key(incident: &Incident) -> String {
    for evidence in &incident.evidence {
        let engine = evidence
            .fields
            .get("engine")
            .and_then(|value| value.as_str());
        let container = evidence
            .fields
            .get("container_id")
            .and_then(|value| value.as_str());
        if let (Some(engine), Some(container)) = (engine, container) {
            return format!("container:{engine}:{container}");
        }
    }
    incident.fingerprint()
}

fn ledger_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("repair-ledger.jsonl")
}

pub(crate) async fn dismiss_incident(incident_dir: &Path, incident: &Incident) -> Result<()> {
    record_incident_status(
        incident_dir,
        incident,
        rescueloop_core::IncidentStatus::Superseded,
        Some(serde_json::json!({"dismissed_by_user": true})),
    )
    .await
}

pub(crate) async fn record_incident_status(
    incident_dir: &Path,
    incident: &Incident,
    status: rescueloop_core::IncidentStatus,
    detail: Option<serde_json::Value>,
) -> Result<()> {
    rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: detail,
            verifier: None,
            status,
            relation_override: None,
        },
    )
    .await?;
    Ok(())
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

pub(crate) async fn repair(
    incident_dir: &Path,
    incident_path: &Path,
    analysis_path: &Path,
    action_index: usize,
    allowed_roots: Vec<PathBuf>,
    approved: bool,
) -> Result<()> {
    let incident: Incident = serde_json::from_slice(&fs::read(incident_path).await?)?;
    let analysis: rescueloop_core::AnalysisResponse =
        serde_json::from_slice(&fs::read(analysis_path).await?)?;
    let proposal = analysis
        .proposed_actions
        .get(action_index)
        .context("action index is out of range")?;
    if let Some(action) = rescueloop_repair::compile_operational(proposal)? {
        println!("DRY RUN: {}", serde_json::to_string_pretty(&action)?);
        if !approved {
            println!("No changes made. Approve this exact operational target to execute.");
            return Ok(());
        }
        let target_id = match &action {
            rescueloop_repair::OperationalAction::RestartContainer { container_id, .. } => {
                container_id.clone()
            }
            rescueloop_repair::OperationalAction::RestartService { service_id } => {
                service_id.clone()
            }
        };
        let evidenced = incident.evidence.iter().any(|evidence| {
            evidence
                .fields
                .values()
                .any(|value| value.as_str() == Some(target_id.as_str()))
        });
        if !evidenced {
            anyhow::bail!("operational target is not present in incident evidence")
        }
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::RepairApplied,
            None,
        )
        .await?;
        let receipt = rescueloop_repair::execute_operational(action, &target_id).await?;
        record_incident_status(
            incident_dir,
            &incident,
            rescueloop_core::IncidentStatus::VerificationPending,
            None,
        )
        .await?;
        let transaction_root = incident_dir
            .parent()
            .unwrap_or(incident_dir)
            .join("transactions")
            .join(receipt.id.to_string());
        fs::create_dir_all(&transaction_root).await?;
        let receipt_path = transaction_root.join("operational-receipt.json");
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?).await?;
        let status = if receipt.verified {
            rescueloop_core::IncidentStatus::VerifiedFixed
        } else {
            rescueloop_core::IncidentStatus::VerificationFailed
        };
        record_incident_status(
            incident_dir,
            &incident,
            status,
            Some(serde_json::to_value(&receipt)?),
        )
        .await?;
        if !receipt.verified {
            anyhow::bail!("operational repair failed verification")
        }
        println!(
            "VERIFIED operational repair. Receipt: {}",
            receipt_path.display()
        );
        return Ok(());
    }
    let plan = rescueloop_repair::compile(proposal)?;
    let proposed_target = std::fs::canonicalize(plan.action.target()).with_context(|| {
        format!(
            "repair target does not exist: {}",
            plan.action.target().display()
        )
    })?;
    let target_is_evidenced = incident.evidence.iter().any(|evidence| {
        evidence
            .artifact
            .as_ref()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .is_some_and(|path| path == proposed_target)
    });
    if !target_is_evidenced {
        anyhow::bail!(
            "filesystem repair target is not the exact artifact recorded in incident evidence"
        )
    }
    let policy = rescueloop_repair::ScopePolicy::new(allowed_roots)?;
    let transaction_root = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("transactions");
    let mut transaction = rescueloop_repair::prepare(&plan, &policy, &transaction_root).await?;
    println!("DRY RUN: {}", serde_json::to_string_pretty(&transaction)?);
    if !approved {
        println!("No changes made. Review the exact target and repeat with --approve to execute.");
        return Ok(());
    }
    let launch_context = incident
        .launch_context
        .clone()
        .context("verified repair requires an exact recorded launch context")?;
    rescueloop_repair::apply(&mut transaction).await?;
    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::RepairApplied,
        None,
    )
    .await?;
    rescueloop_repair::persist(&transaction, &transaction_root).await?;
    println!(
        "APPLIED: backup created at {}",
        transaction.backup.display()
    );

    record_incident_status(
        incident_dir,
        &incident,
        rescueloop_core::IncidentStatus::VerificationPending,
        None,
    )
    .await?;
    let replay = rescueloop_platform::verify_replay(&launch_context).await;
    match replay {
        Ok(result) if result.passed => {
            rescueloop_repair::finalize(&mut transaction, true).await?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            println!(
                "VERIFIED: original action now succeeds ({} ms).",
                result.duration_ms
            );
            println!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::VerifiedFixed,
                serde_json::json!({"passed": true, "exit_code": result.exit_code, "duration_ms": result.duration_ms}),
            )
            .await?;
        }
        result => {
            let replay_message = match result {
                Ok(value) => format!("exit code {:?}", value.exit_code),
                Err(error) => error.to_string(),
            };
            rescueloop_repair::finalize(&mut transaction, false)
                .await
                .with_context(|| {
                    format!(
                        "CRITICAL: verification failed ({replay_message}) and automatic rollback also failed"
                    )
                })?;
            let receipt = rescueloop_repair::persist(&transaction, &transaction_root).await?;
            println!(
                "ROLLED BACK: verification failed ({replay_message}); original state restored."
            );
            println!("Transaction receipt: {}", receipt.display());
            record_repair_lineage(
                incident_dir,
                &incident,
                &transaction,
                rescueloop_core::IncidentStatus::RolledBack,
                serde_json::json!({"passed": false, "detail": replay_message}),
            )
            .await?;
        }
    }
    Ok(())
}

async fn record_repair_lineage(
    incident_dir: &Path,
    incident: &Incident,
    transaction: &rescueloop_repair::Transaction,
    status: rescueloop_core::IncidentStatus,
    verifier: serde_json::Value,
) -> Result<()> {
    let entry = rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: Some(serde_json::to_value(&transaction.action)?),
            before_state: Some(serde_json::json!({"original": transaction.original})),
            after_state: Some(serde_json::json!({"backup": transaction.backup, "transaction_state": transaction.state})),
            verifier: Some(verifier),
            status,
            relation_override: None,
        },
    )
    .await?;
    println!("LINEAGE: {:?}", entry.relation);
    Ok(())
}
