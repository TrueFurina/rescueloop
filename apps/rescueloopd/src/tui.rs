use crate::{
    analyze_with_provider, configured_provider, dismiss_incident, incidents, local_timestamp,
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
    text::{Line, Span},
    widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use rescueloop_core::{AnalysisResponse, Incident, IncidentStatus};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use uuid::Uuid;

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
                        .is_some_and(|value| !value.proposed_actions.is_empty()) =>
                {
                    app.show_repair = true;
                    app.state = UiState::ConfirmRepair;
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

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let wide_footer = area.width >= 170;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(if wide_footer { 3 } else { 4 }),
        ])
        .split(area);
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " RescueLoop ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  LIVE  •  AI: {}", app.agent_name)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    let detail_height = if app.show_details || app.show_repair || app.analysis.is_some() {
        Constraint::Percentage(42)
    } else {
        Constraint::Length(10)
    };
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), detail_height])
        .split(chunks[1]);
    let rows = app
        .incidents
        .iter()
        .map(|(incident, _)| {
            Row::new(vec![
                Cell::from(
                    incident
                        .application
                        .as_deref()
                        .unwrap_or("Unknown application"),
                ),
                Cell::from(format!("{:?}", incident.kind)),
                Cell::from(incident_source_label(incident)),
                Cell::from(format!("×{}", incident.occurrence_count)),
                Cell::from(local_timestamp(
                    incident.last_observed_at.unwrap_or(incident.observed_at),
                )),
                Cell::from(format!("{:?}", incident.status)),
            ])
        })
        .collect::<Vec<_>>();
    let header = Row::new([
        "APPLICATION",
        "PROBLEM",
        "SOURCE",
        "COUNT",
        "LOCAL TIME",
        "STATUS",
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Fill(5),
            Constraint::Fill(3),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(" Incidents ({}) ", app.incidents.len()))
            .borders(Borders::ALL),
    )
    .row_highlight_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    let table = table.highlight_spacing(HighlightSpacing::Always);
    let mut table_state =
        TableState::default().with_selected((!app.incidents.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(table, body[0], &mut table_state);

    let detail = detail_text(app);
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(if app.show_details {
                    " Problem details — [Esc] Close "
                } else {
                    " Incident preview — [Enter] More details "
                })
                .borders(Borders::ALL),
        ),
        body[1],
    );
    let footer = match app.state {
        UiState::ConfirmAnalysis {
            replace_saved: false,
        } => " Send scrubbed evidence to AI?  [y] Yes  [n] No ".to_string(),
        UiState::ConfirmAnalysis {
            replace_saved: true,
        } => " Replace the saved analysis with a fresh AI result?  [y] Re-analyze  [n] Keep saved ".to_string(),
        UiState::ConfirmRepair => " Apply this reversible repair, replay the app, and auto-rollback on failure?  [y] Apply  [n] Cancel ".to_string(),
        UiState::ConfirmQuit => {
            " Disconnect the console? The background watcher will keep running.  [y] Exit  [n] Stay ".to_string()
        }
        UiState::Analyzing { started } => {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let index = (started.elapsed().as_millis() / 100) as usize % frames.len();
            format!(
                " {} AI is analyzing evidence… {:.1}s ",
                frames[index],
                started.elapsed().as_secs_f32()
            )
        }
        UiState::Repairing { started } => {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let index = (started.elapsed().as_millis() / 100) as usize % frames.len();
            format!(" {} Applying repair, verifying, and protecting rollback… {:.1}s ", frames[index], started.elapsed().as_secs_f32())
        }
        UiState::Gathering { started } => {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let index = (started.elapsed().as_millis() / 100) as usize % frames.len();
            format!(" {} Reproducing failure and collecting evidence… {:.1}s ", frames[index], started.elapsed().as_secs_f32())
        }
        _ => {
            let context_action = if app
                .analysis
                .as_ref()
                .is_some_and(|analysis| !analysis.proposed_actions.is_empty())
            {
                "[R] Apply fix"
            } else if app.analysis.as_ref().is_some_and(|analysis| {
                analysis.needs_more_evidence && analysis.proposed_actions.is_empty()
            }) {
                "[G] Gather evidence"
            } else {
                ""
            };
            let history = if app.show_history {
                "[H] Active issues"
            } else {
                "[H] History"
            };
            let analysis_action = if app.analysis.is_some() {
                "[A] Saved result"
            } else {
                "[A] Analyze"
            };
            let refresh_action = if app.analysis.is_some() {
                "[U] Re-analyze"
            } else {
                ""
            };
            let first = format!(
                " {:<18}{:<23}{:<20}{:<22}{:<22}",
                "[↑↓] Select",
                "[Enter] Details",
                analysis_action,
                refresh_action,
                context_action
            );
            let second = format!(
                "{:<18}{:<23}{:<17}",
                "[D] Dismiss", history, "[Q] Disconnect"
            );
            if wide_footer {
                format!("{first}{second}")
            } else {
                format!("{first}\n {second}")
            }
        }
    };
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL)),
        chunks[2],
    );
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

fn detail_text(app: &App) -> String {
    if let UiState::Message(message) = &app.state {
        return message.clone();
    }
    let Some((incident, _)) = app.incidents.get(app.selected) else {
        return "Waiting for an objective failure…".into();
    };
    if let Some(analysis) = &app.analysis {
        let mut text = format!("AI DIAGNOSIS\n\n{}\n\n", analysis.summary);
        if analysis.proposed_actions.is_empty() {
            text.push_str(if analysis.needs_more_evidence {
                "NO SAFE FIX PROPOSED\nMore evidence is required. Nothing was changed."
            } else {
                "NO APPLICABLE REPAIR FOUND\nNothing was changed."
            });
        } else {
            text.push_str(&format!(
                "PROPOSED FIX\n{}\n\nPress [R] to inspect the exact change and approve it.",
                analysis.proposed_actions[0].action_type
            ));
            if app.show_repair {
                text.push_str(&format!(
                    "\n\nReason: {}\nParameters: {}\nReversible: {}",
                    analysis.proposed_actions[0].reason,
                    analysis.proposed_actions[0].parameters,
                    analysis.proposed_actions[0].reversible
                ));
            }
        }
        return text;
    }
    if app.show_details {
        return format!(
            "PROBLEM\n{}\n\nAPPLICATION\n{}\n\nSOURCE\n{}\n\nTYPE\n{:?}\n\nSTATUS\n{:?}\n\nOCCURRENCES\n{}\n\nFIRST DETECTED\n{}\n\nLAST DETECTED\n{}\n\nCONFIDENCE\n{:?}\n\nEVIDENCE\n{}",
            incident.message,
            incident
                .application
                .as_deref()
                .unwrap_or("Unknown application"),
            incident_source_label(incident),
            incident.kind,
            incident.status,
            incident.occurrence_count,
            local_timestamp(incident.first_observed_at.unwrap_or(incident.observed_at)),
            local_timestamp(incident.last_observed_at.unwrap_or(incident.observed_at)),
            incident.confidence,
            serde_json::to_string_pretty(&incident.evidence).unwrap_or_default()
        );
    }
    format!(
        "{}\n\nSource: {}\nStatus: {:?}\nFailure: {:?}\nConfidence: {:?}\nObserved: {}\n\n{}",
        incident
            .application
            .as_deref()
            .unwrap_or("Unknown application"),
        incident_source_label(incident),
        incident.status,
        incident.kind,
        incident.confidence,
        local_timestamp(incident.observed_at),
        incident.message
    )
}

fn incident_source_label(incident: &Incident) -> String {
    if let Some(engine) = incident.evidence.iter().find_map(|evidence| {
        evidence
            .fields
            .get("engine")
            .and_then(serde_json::Value::as_str)
    }) {
        let mut characters = engine.chars();
        return characters
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
            .unwrap_or_else(|| "Container".into());
    }
    let source = incident
        .evidence
        .first()
        .map(|evidence| evidence.source.as_str())
        .unwrap_or_default();
    if source.starts_with("macos") {
        "macOS".into()
    } else if source.starts_with("windows") {
        "Windows".into()
    } else if source == "supervised-process" {
        "Process".into()
    } else {
        "System".into()
    }
}

/// Terminal protocols normally expose the produced character, not the physical
/// key. Map Ukrainian/Russian keyboard-layout characters back to RescueLoop's
/// Latin hotkeys so switching layouts does not make the TUI appear frozen.
fn normalize_key_code(code: KeyCode) -> KeyCode {
    let KeyCode::Char(character) = code else {
        return code;
    };
    let character = character.to_lowercase().next().unwrap_or(character);
    let latin = match character {
        'й' => 'q',
        'ф' => 'a',
        'к' => 'r',
        'р' => 'h',
        'в' => 'd',
        'п' => 'g',
        'г' => 'u',
        'н' => 'y',
        'т' => 'n',
        'о' => 'j',
        'л' => 'k',
        value if value.is_ascii_alphabetic() => value,
        _ => return KeyCode::Char(character),
    };
    KeyCode::Char(latin)
}

#[cfg(test)]
mod tests {
    use super::normalize_key_code;
    use crossterm::event::KeyCode;

    #[test]
    fn maps_cyrillic_layout_hotkeys() {
        for (input, expected) in [
            ('й', 'q'),
            ('ф', 'a'),
            ('к', 'r'),
            ('р', 'h'),
            ('в', 'd'),
            ('п', 'g'),
            ('г', 'u'),
            ('н', 'y'),
            ('т', 'n'),
            ('о', 'j'),
            ('л', 'k'),
            ('Й', 'q'),
        ] {
            assert_eq!(
                normalize_key_code(KeyCode::Char(input)),
                KeyCode::Char(expected)
            );
        }
    }

    #[test]
    fn preserves_navigation_and_normalizes_latin_case() {
        assert_eq!(normalize_key_code(KeyCode::Up), KeyCode::Up);
        assert_eq!(normalize_key_code(KeyCode::Char('Q')), KeyCode::Char('q'));
    }
}
