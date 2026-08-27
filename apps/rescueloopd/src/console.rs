use anyhow::{Context, Result};
use rescueloop_agent::{AgentConfig, CliAnalysisProvider, HttpAnalysisProvider};
use rescueloop_core::AnalysisProvider;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;

use crate::incident_store::{
    incident_and_path_by_number, incident_by_number, incident_index, incidents, local_timestamp,
    print_incidents,
};
use crate::{IndexAction, SourcesAction, analyze_with_provider, repair, replay, service};

const SOURCE_NAMES: &[&str] = &["system-artifacts", "containers", "os-log"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    #[serde(default = "default_sources")]
    pub(crate) enabled_sources: Vec<String>,
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

pub(crate) async fn console(
    dir: &Path,
    endpoint: Option<String>,
    token: Option<String>,
) -> Result<()> {
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

pub(crate) async fn setup(incident_dir: &Path) -> Result<()> {
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

pub(crate) async fn load_settings(incident_dir: &Path) -> Result<Settings> {
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

pub(crate) async fn sources(incident_dir: &Path, action: SourcesAction) -> Result<()> {
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

pub(crate) async fn index_command(incident_dir: &Path, action: IndexAction) -> Result<()> {
    let index = incident_index(incident_dir).await?;
    match action {
        IndexAction::Status => {
            println!("Index: {}", index.path().display());
            println!("Schema: v1 (disposable projection)");
            println!("Indexed incidents: {}", index.count().await?);
            println!("Source of truth: {}", incident_dir.display());
        }
        IndexAction::Rebuild => {
            let count = index.rebuild().await?;
            println!("Rebuilt index from {count} versioned JSON incident(s).");
            println!("No source JSON was modified.");
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
