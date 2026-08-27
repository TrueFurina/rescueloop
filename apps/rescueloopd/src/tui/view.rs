use super::{App, UiState};
use crate::local_timestamp;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use rescueloop_core::Incident;

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
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
