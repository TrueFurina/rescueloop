use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rescueloop_agent::{ALLOWED_ACTIONS, AgentConfig, CliAnalysisProvider, HttpAnalysisProvider};
use rescueloop_core::{AnalysisProvider, AnalysisRequest, Incident};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tracing::info;

mod tui;

#[derive(Parser)]
#[command(
    name = "rescueloop",
    about = "Detect failures first; analyze only with explicit user intent"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[arg(long, default_value = ".rescueloop/incidents", global = true)]
    incident_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Monitor OS diagnostic artifacts and persist normalized incidents.
    Watch,
    /// Detect installed AI agents and save the selected provider.
    Setup,
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
        #[arg(long, required = true)]
        allow_root: Vec<PathBuf>,
        #[arg(long)]
        approve: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Watch => watch(&cli.incident_dir).await,
        Command::Setup => setup(&cli.incident_dir).await,
        Command::Console {
            endpoint,
            token,
            plain,
        } => {
            if plain {
                console(&cli.incident_dir, endpoint, token).await
            } else {
                tui::run(cli.incident_dir, endpoint, token).await
            }
        }
        Command::Analyze {
            incident,
            endpoint,
            token,
            output,
        } => analyze(&incident, endpoint, token, output.as_deref()).await,
        Command::Run {
            record_args,
            executable,
            args,
        } => run_supervised(&cli.incident_dir, executable, args, record_args).await,
        Command::Replay { incident } => replay(&incident).await,
        Command::Repair {
            incident,
            analysis,
            action_index,
            allow_root,
            approve,
        } => {
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
                let Some(target) = proposal
                    .parameters
                    .get("target")
                    .and_then(|value| value.as_str())
                else {
                    println!("This proposal cannot be executed by the guided MVP yet.");
                    continue;
                };
                let target = PathBuf::from(target);
                let allowed_root = target
                    .parent()
                    .context("repair target has no parent scope")?
                    .to_path_buf();
                println!("\nSafety review (no changes yet):");
                repair(dir, &path, &output, 0, vec![allowed_root.clone()], false).await?;
                if confirm("Apply this exact repair and replay the original action? [y/N] ")? {
                    repair(dir, &path, &output, 0, vec![allowed_root], true).await?;
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
        return Ok(());
    }
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
            "Select exactly one agent [1-{}], or q to cancel: ",
            detected.len()
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let answer = answer.trim();
        if answer.eq_ignore_ascii_case("q") {
            println!("Setup cancelled; no agent was selected.");
            return Ok(());
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
        break config;
    };
    let path = config_path(incident_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, serde_json::to_vec_pretty(config)?).await?;
    println!("Selected: {:?}", config.agent);
    println!("Configuration saved to {}", path.display());
    println!("The agent runs read-only; Repair IR is validated before approval.");
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
    file.write_all(&serde_json::to_vec_pretty(incident)?)
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

fn ledger_path(incident_dir: &Path) -> PathBuf {
    incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("repair-ledger.jsonl")
}

pub(crate) async fn dismiss_incident(incident_dir: &Path, incident: &Incident) -> Result<()> {
    rescueloop_ledger::append(
        &ledger_path(incident_dir),
        rescueloop_ledger::NewLedgerEntry {
            incident: incident.clone(),
            repair: None,
            before_state: None,
            after_state: Some(serde_json::json!({"dismissed_by_user": true})),
            verifier: None,
            status: rescueloop_core::IncidentStatus::Superseded,
            relation_override: None,
        },
    )
    .await?;
    Ok(())
}

async fn watch(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).await?;
    let mut collector = rescueloop_platform::system_collector()?;
    info!(collector = collector.name(), "failure detector started");
    println!("RescueLoop {}", env!("CARGO_PKG_VERSION"));
    println!("Status: READY — monitoring for objective failures");
    println!(
        "Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Collector: {}", collector.name());
    println!("Incidents: {}", dir.display());
    println!("Privacy: local detection only; AI analysis starts only on request");
    println!("Waiting for a new crash or failure report...\n");
    loop {
        let incident = collector.next_incident().await?;
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
    let mut incident: Incident =
        serde_json::from_slice(&fs::read(path).await.context("cannot read incident")?)
            .context("invalid incident JSON")?;
    // Local artifact locations may contain usernames or private directory names.
    // The allowlisted diagnostic metadata is enough for the provider contract.
    for evidence in &mut incident.evidence {
        evidence.artifact = None;
    }
    if let Some(context) = &mut incident.launch_context {
        context.arguments = None;
        context.working_directory = None;
        context.executable = context
            .executable
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_default();
    }
    let request = AnalysisRequest {
        schema_version: 1,
        incident,
        allowed_actions: ALLOWED_ACTIONS.iter().map(|x| x.to_string()).collect(),
    };
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
    let plan = rescueloop_repair::compile(proposal)?;
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
    rescueloop_repair::persist(&transaction, &transaction_root).await?;
    println!(
        "APPLIED: backup created at {}",
        transaction.backup.display()
    );

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
