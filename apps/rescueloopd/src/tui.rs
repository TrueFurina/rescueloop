use crate::console::save_agent_config;
use crate::{
    analyze_with_provider, configured_provider, dismiss_incident, incidents,
    record_incident_status, repair_silent,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use rescueloop_agent::AgentConfig;
use rescueloop_core::{AnalysisResponse, Incident, IncidentStatus};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use uuid::Uuid;

mod keys;
mod view;

use keys::normalize_key_code;
use view::draw;

enum UiState {
    Ready,
    ConfirmAnalysis { replace_saved: bool },
    ConfirmRepair,
    ConfirmQuit,
    Analyzing { started: Instant },
    Repairing { started: Instant },
    Gathering { started: Instant },
    Message(String),
}

struct App {
    incidents: Vec<(Incident, PathBuf)>,
    selected: usize,
    show_details: bool,
    show_repair: bool,
    state: UiState,
    analysis: Option<AnalysisResponse>,
    agent_name: String,
    show_history: bool,
}

pub async fn run(dir: PathBuf, endpoint: Option<String>, token: Option<String>) -> Result<()> {
    let provider = configured_provider(&dir, endpoint.clone(), token.clone()).await?;
    let needs_agent_onboarding = provider.is_none() && endpoint.is_none();
    let agent_name = provider
        .as_ref()
        .map(|value| value.name().to_string())
        .unwrap_or_else(|| "not configured — run `rescueloop setup`".into());
    drop(provider);
    let initial_incidents = visible_incidents(incidents(&dir).await?, false);
    let initial_analysis = match initial_incidents.first() {
        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
        None => None,
    };
    let mut app = App {
        incidents: initial_incidents,
        selected: 0,
        show_details: false,
        show_repair: false,
        state: UiState::Ready,
        analysis: initial_analysis,
        agent_name,
        show_history: false,
    };
    let (sender, mut results) =
        mpsc::unbounded_channel::<(Uuid, Result<AnalysisResponse, String>)>();
    let (repair_sender, mut repair_results) = mpsc::unbounded_channel::<Result<String, String>>();
    let (gather_sender, mut gather_results) =
        mpsc::unbounded_channel::<(Uuid, Result<String, String>)>();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let outcome = async {
        if needs_agent_onboarding {
            let Some(selected) = select_agent(&mut terminal, &dir).await? else {
                return Ok(());
            };
            app.agent_name = format!("{:?}", selected.agent);
        }
        let mut last_refresh = Instant::now();
        loop {
            terminal.draw(|frame| draw(frame, &app))?;
            if let Ok((incident_id, result)) = results.try_recv()
                && app
                    .incidents
                    .get(app.selected)
                    .is_some_and(|item| item.0.id == incident_id)
            {
                match result {
                    Ok(analysis) => {
                        let status = if analysis.proposed_actions.is_empty() {
                            IncidentStatus::Diagnosed
                        } else {
                            IncidentStatus::RepairProposed
                        };
                        if let Some((incident, _)) = app.incidents.get(app.selected) {
                            record_incident_status(&dir, incident, status, None).await?;
                        }
                        app.analysis = Some(analysis);
                        app.state = UiState::Ready;
                    }
                    Err(error) => {
                        if let Some((incident, _)) = app.incidents.get(app.selected) {
                            record_incident_status(
                                &dir,
                                incident,
                                IncidentStatus::Detected,
                                Some(serde_json::json!({"analysis_error": error.clone()})),
                            )
                            .await?;
                        }
                        app.state = UiState::Message(format!("AI analysis failed safely:\n{error}"))
                    }
                }
            }
            if let Ok(result) = repair_results.try_recv() {
                app.state = UiState::Message(match result {
                    Ok(message) => message,
                    Err(error) => format!("REPAIR FAILED SAFELY\n\n{error}\n\nNo unverified change was retained."),
                });
            }
            if let Ok((incident_id, result)) = gather_results.try_recv()
                && app
                    .incidents
                    .get(app.selected)
                    .is_some_and(|item| item.0.id == incident_id)
            {
                app.state = UiState::Message(match result {
                    Ok(message) => message,
                    Err(error) => format!("COULD NOT COLLECT EVIDENCE\n\n{error}"),
                });
            }
            if last_refresh.elapsed() >= Duration::from_secs(2) {
                let newest_before = app.incidents.first().map(|item| item.0.id);
                let refreshed = visible_incidents(incidents(&dir).await?, app.show_history);
                if refreshed.first().map(|item| item.0.id) != newest_before {
                    app.selected = 0;
                    app.analysis = match refreshed.first() {
                        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
                        None => None,
                    };
                }
                app.incidents = refreshed;
                last_refresh = Instant::now();
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let key_code = normalize_key_code(key.code);
            match (&app.state, key_code) {
                (UiState::ConfirmQuit, KeyCode::Char('y')) => break,
                (UiState::ConfirmQuit, KeyCode::Char('n') | KeyCode::Esc) => {
                    app.state = UiState::Ready
                }
                (UiState::ConfirmAnalysis { .. }, KeyCode::Char('n') | KeyCode::Esc) => {
                    app.state = UiState::Ready
                }
                (UiState::ConfirmRepair, KeyCode::Char('n') | KeyCode::Esc) => {
                    app.state = UiState::Ready
                }
                (UiState::ConfirmAnalysis { .. }, KeyCode::Char('y')) => {
                    let Some((incident, path)) = app.incidents.get(app.selected).cloned() else {
                        continue;
                    };
                    let Some(provider) =
                        configured_provider(&dir, endpoint.clone(), token.clone()).await?
                    else {
                        app.state = UiState::Message(
                            "No AI agent configured. Exit and run `rescueloop setup`.".into(),
                        );
                        continue;
                    };
                    let output_dir = dir.parent().unwrap_or(&dir).join("analyses");
                    tokio::fs::create_dir_all(&output_dir).await?;
                    let output = output_dir.join(format!("{}.json", incident.id));
                    let tx = sender.clone();
                    record_incident_status(
                        &dir,
                        &incident,
                        IncidentStatus::Investigating,
                        None,
                    )
                    .await?;
                    tokio::spawn(async move {
                        let result = analyze_with_provider(&path, provider.as_ref(), Some(&output))
                            .await
                            .map_err(|e| e.to_string());
                        let _ = tx.send((incident.id, result));
                    });
                    app.state = UiState::Analyzing {
                        started: Instant::now(),
                    };
                }
                (UiState::ConfirmRepair, KeyCode::Char('y')) => {
                    let Some((incident, incident_path)) = app.incidents.get(app.selected).cloned()
                    else {
                        continue;
                    };
                    let Some(analysis) = app.analysis.as_ref() else {
                        continue;
                    };
                    let Some(proposal) = analysis.proposed_actions.first() else {
                        continue;
                    };
                    let target = proposal.parameters.get("target").and_then(|v| v.as_str()).map(PathBuf::from);
                    let allowed_roots = target.as_ref().and_then(|path| path.parent()).map(PathBuf::from).into_iter().collect();
                    let analysis_path = dir.parent().unwrap_or(&dir).join("analyses").join(format!("{}.json", app.incidents[app.selected].0.id));
                    let incident_dir = dir.clone();
                    let tx = repair_sender.clone();
                    tokio::spawn(async move {
                        let result = if target.as_ref().is_some_and(|target| !target.exists()) {
                            match incident.launch_context.as_ref() {
                                Some(context) => match rescueloop_platform::verify_replay(context).await {
                                    Ok(replay) if replay.passed => Ok(
                                        "ALREADY RESOLVED\n\nThe proposed target is already absent and the original action now succeeds. No additional change was needed."
                                            .to_string(),
                                    ),
                                    Ok(replay) => Err(format!(
                                        "The proposed target is already absent, but replay still fails with exit code {:?}. Run AI analysis again for the current state.",
                                        replay.exit_code
                                    )),
                                    Err(error) => Err(format!(
                                        "The proposed target is already absent and replay could not be verified: {error}"
                                    )),
                                },
                                None => Err(
                                    "The proposed target is already absent. This repair proposal is stale, and the incident has no recorded launch context for verification."
                                        .to_string(),
                                ),
                            }
                        } else {
                            repair_silent(&incident_dir, &incident_path, &analysis_path, 0, allowed_roots, true)
                                .await
                                .map(|_| "REPAIR WORKFLOW FINISHED\n\nThe original action was replayed. The repair was verified or automatically rolled back; a transaction receipt was saved.".to_string())
                                .map_err(|e| e.to_string())
                        };
                        let _ = tx.send(result);
                    });
                    app.state = UiState::Repairing { started: Instant::now() };
                }
                (
                    UiState::Analyzing { .. }
                    | UiState::Repairing { .. }
                    | UiState::Gathering { .. },
                    _,
                ) => {}
                (_, KeyCode::Char('q')) => app.state = UiState::ConfirmQuit,
                (_, KeyCode::Up | KeyCode::Char('k')) => {
                    app.selected = app.selected.saturating_sub(1);
                    app.analysis = match app.incidents.get(app.selected) {
                        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
                        None => None,
                    };
                    app.show_repair = false;
                }
                (_, KeyCode::Down | KeyCode::Char('j')) => {
                    if app.selected + 1 < app.incidents.len() {
                        app.selected += 1;
                    }
                    app.analysis = match app.incidents.get(app.selected) {
                        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
                        None => None,
                    };
                    app.show_repair = false;
                }
                (_, KeyCode::Enter) => app.show_details = !app.show_details,
                (_, KeyCode::Char('a')) if app.analysis.is_none() => {
                    app.state = UiState::ConfirmAnalysis {
                        replace_saved: false,
                    };
                }
                (_, KeyCode::Char('a')) => {
                    app.state = UiState::Ready;
                }
                (_, KeyCode::Char('u')) if app.analysis.is_some() => {
                    app.state = UiState::ConfirmAnalysis {
                        replace_saved: true,
                    };
                }
                (_, KeyCode::Char('h')) => {
                    app.show_history = !app.show_history;
                    app.incidents = visible_incidents(incidents(&dir).await?, app.show_history);
                    app.selected = app.selected.min(app.incidents.len().saturating_sub(1));
                    app.analysis = match app.incidents.get(app.selected) {
                        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
                        None => None,
                    };
                }
                (_, KeyCode::Char('d')) => {
                    let Some((incident, _)) = app.incidents.get(app.selected).cloned() else {
                        continue;
                    };
                    dismiss_incident(&dir, &incident).await?;
                    app.incidents = visible_incidents(incidents(&dir).await?, app.show_history);
                    app.selected = app.selected.min(app.incidents.len().saturating_sub(1));
                    app.analysis = match app.incidents.get(app.selected) {
                        Some((incident, _)) => load_saved_analysis(&dir, incident.id).await?,
                        None => None,
                    };
                    app.state = UiState::Message(
                        "DISMISSED\n\nThis item was marked as not actionable and removed from active issues. It remains available in History.".into(),
                    );
                }
                (_, KeyCode::Char('g'))
                    if app.analysis.as_ref().is_some_and(|value| {
                        value.needs_more_evidence && value.proposed_actions.is_empty()
                    }) =>
                {
                    let Some((incident, path)) = app.incidents.get(app.selected).cloned() else {
                        continue;
                    };
                    let Some(context) = incident.launch_context.clone() else {
                        app.state = UiState::Message(
                            "More evidence is needed, but this incident has no recorded launch context. Reproduce it with `rescueloop run --record-args <program>`.".into(),
                        );
                        continue;
                    };
                    let Some(args) = context.arguments.clone() else {
                        app.state = UiState::Message(
                            "Exact arguments were not recorded. Reproduce it with `rescueloop run --record-args <program>`.".into(),
                        );
                        continue;
                    };
                    let tx = gather_sender.clone();
                    let incident_id = incident.id;
                    tokio::spawn(async move {
                        let result = rescueloop_platform::supervise_quiet(
                            &context.executable,
                            &args,
                            true,
                        )
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|fresh| match fresh {
                                Some(fresh) => {
                                    let mut enriched = incident;
                                    enriched.evidence.extend(fresh.evidence);
                                    enriched.normalized_failure = fresh.normalized_failure;
                                    std::fs::write(&path, serde_json::to_vec_pretty(&enriched).map_err(|e| e.to_string())?)
                                        .map_err(|e| e.to_string())?;
                                    Ok("NEW EVIDENCE COLLECTED\n\nThe failure was reproduced and its latest diagnostic output was attached. Press [A] to analyze again.".to_string())
                                }
                                None => Ok("ISSUE NO LONGER REPRODUCES\n\nThe recorded action now succeeds. No repair is currently needed.".to_string()),
                            });
                        let _ = tx.send((incident_id, result));
                    });
                    app.state = UiState::Gathering { started: Instant::now() };
                }
                (_, KeyCode::Char('r'))
                    if app
                        .analysis
                        .as_ref()
                        .is_some_and(|value| !value.proposed_actions.is_empty())
                        && app.show_repair =>
                {
                    app.state = UiState::ConfirmRepair;
                }
                (_, KeyCode::Char('r'))
                    if app
                        .analysis
                        .as_ref()
                        .is_some_and(|value| !value.proposed_actions.is_empty()) =>
                {
                    app.show_repair = true;
                    app.state = UiState::Ready;
                }
                (_, KeyCode::Esc) => {
                    app.show_details = false;
                    app.show_repair = false;
                    app.state = UiState::Ready;
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

async fn select_agent(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dir: &std::path::Path,
) -> Result<Option<AgentConfig>> {
    let agents = rescueloop_agent::detect_cli_agents();
    let mut selected = 0_usize;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Min(3),
                    Constraint::Length(3),
                ])
                .split(area);
            frame.render_widget(
                Paragraph::new(
                    "Welcome to RescueLoop\nChoose the local AI agent used for diagnosis. Repairs remain deterministic and require explicit approval.",
                )
                .block(Block::default().borders(Borders::ALL).title(" First run "))
                .wrap(Wrap { trim: true }),
                sections[0],
            );
            if agents.is_empty() {
                frame.render_widget(
                    Paragraph::new(
                        "No supported OpenAI Codex or Claude CLI was detected. Install one and restart RescueLoop, or press Esc to close.",
                    )
                    .block(Block::default().borders(Borders::ALL).title(" AI agents "))
                    .wrap(Wrap { trim: true }),
                    sections[1],
                );
            } else {
                let items = agents
                    .iter()
                    .enumerate()
                    .map(|(index, agent)| {
                        ListItem::new(format!(
                            "[{}] {:?} — {}",
                            index + 1,
                            agent.agent,
                            agent.executable.display()
                        ))
                    })
                    .collect::<Vec<_>>();
                let mut state = ListState::default().with_selected(Some(selected));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(Block::default().borders(Borders::ALL).title(" AI agents "))
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("▶ "),
                    sections[1],
                    &mut state,
                );
            }
            frame.render_widget(
                Paragraph::new(if agents.is_empty() {
                    "Esc or q: close RescueLoop"
                } else {
                    "↑/↓ or j/k: select    Enter: confirm    Esc or q: close RescueLoop"
                })
                .block(Block::default().borders(Borders::ALL)),
                sections[2],
            );
        })?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match normalize_key_code(key.code) {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Up | KeyCode::Char('k') if !agents.is_empty() => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if !agents.is_empty() => {
                selected = (selected + 1).min(agents.len() - 1);
            }
            KeyCode::Char(value) if value.is_ascii_digit() && value != '0' => {
                let index = value.to_digit(10).unwrap_or_default() as usize - 1;
                if let Some(agent) = agents.get(index) {
                    save_agent_config(dir, agent).await?;
                    return Ok(Some(agent.clone()));
                }
            }
            KeyCode::Enter => {
                if let Some(agent) = agents.get(selected) {
                    save_agent_config(dir, agent).await?;
                    return Ok(Some(agent.clone()));
                }
            }
            _ => {}
        }
    }
}

async fn load_saved_analysis(
    dir: &std::path::Path,
    incident_id: Uuid,
) -> Result<Option<AnalysisResponse>> {
    let path = dir
        .parent()
        .unwrap_or(dir)
        .join("analyses")
        .join(format!("{incident_id}.json"));
    if !tokio::fs::try_exists(&path).await? {
        return Ok(None);
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(analysis) => Ok(Some(analysis)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid saved analysis");
                Ok(None)
            }
        },
        Err(error) => Err(error.into()),
    }
}

fn visible_incidents(
    mut values: Vec<(Incident, PathBuf)>,
    show_history: bool,
) -> Vec<(Incident, PathBuf)> {
    if !show_history {
        values.retain(|(incident, _)| {
            !matches!(
                incident.status,
                IncidentStatus::VerifiedFixed | IncidentStatus::Superseded
            )
        });
    }
    values
}
