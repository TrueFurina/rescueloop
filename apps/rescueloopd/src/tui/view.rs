use super::{App, UiState};
use crate::local_timestamp;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap},
};
use rescueloop_core::{AnalysisResponse, Incident};
use serde_json::Value;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let footer_height = if area.width >= 170 { 3 } else { 5 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(footer_height),
        ])
        .split(area);

    render_header(frame, app, chunks[0]);

    let incident_height = if area.height >= 34 {
        Constraint::Percentage(42)
    } else {
        Constraint::Percentage(48)
    };
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([incident_height, Constraint::Min(9)])
        .split(chunks[1]);
    render_incidents(frame, app, body[0]);
    render_workspace(frame, app, body[1]);
    render_footer(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " RescueLoop ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  LIVE  ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("•", Style::default().fg(Color::Yellow)),
        Span::raw(format!("  AI: {}", app.agent_name)),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, area);
}

fn render_incidents(frame: &mut Frame<'_>, app: &App, area: Rect) {
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
            Constraint::Length(7),
            Constraint::Length(20),
            Constraint::Length(18),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(
                " {} incidents ({}) ",
                if app.show_history {
                    "History"
                } else {
                    "Active"
                },
                app.incidents.len()
            ))
            .borders(Borders::ALL),
    )
    .row_highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
    .highlight_symbol("▶ ")
    .highlight_spacing(HighlightSpacing::Always);
    let mut state =
        TableState::default().with_selected((!app.incidents.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_workspace(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if let UiState::Message(message) = &app.state {
        render_panel(frame, area, " Status ", message.clone(), Color::Yellow);
        return;
    }
    let Some((incident, _)) = app.incidents.get(app.selected) else {
        render_panel(
            frame,
            area,
            " Waiting for an incident ",
            "RescueLoop is watching for objective failures.".into(),
            MUTED,
        );
        return;
    };

    let evidence = evidence_text(incident, app.show_details);
    let Some(analysis) = &app.analysis else {
        let title = if app.show_details {
            " Evidence · [Enter] Collapse "
        } else {
            " Evidence · [Enter] Expand "
        };
        render_panel(frame, area, title, evidence, ACCENT);
        return;
    };

    if area.width >= 112 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
            .split(area);
        render_panel(frame, columns[0], " Evidence ", evidence, ACCENT);
        render_analysis_column(frame, analysis, app.show_repair, columns[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        render_panel(frame, rows[0], " Evidence ", evidence, ACCENT);
        render_analysis_column(frame, analysis, app.show_repair, rows[1]);
    }
}

fn render_analysis_column(
    frame: &mut Frame<'_>,
    analysis: &AnalysisResponse,
    show_repair: bool,
    area: Rect,
) {
    if analysis.proposed_actions.is_empty() {
        let outcome = if analysis.needs_more_evidence {
            "\n\nMore evidence is required. Nothing was changed."
        } else {
            "\n\nNo applicable repair was found. Nothing was changed."
        };
        render_panel(
            frame,
            area,
            " Analysis · saved ",
            format!("{}{}", analysis.summary, outcome),
            ACCENT,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    render_panel(
        frame,
        rows[0],
        " Analysis · saved ",
        analysis.summary.clone(),
        ACCENT,
    );
    let proposal = &analysis.proposed_actions[0];
    let mut plan = format!(
        "1. {}\n2. Verify by replaying the original failure\n3. Roll back if verification fails",
        action_label(&proposal.action_type)
    );
    if show_repair {
        plan.push_str(&format!(
            "\n\nReason: {}\nParameters: {}\nReversible: {}",
            proposal.reason,
            compact_value(&proposal.parameters),
            if proposal.reversible { "yes" } else { "no" }
        ));
    } else {
        plan.push_str("\n\n[R] Review exact plan");
    }
    render_panel(
        frame,
        rows[1],
        if show_repair {
            " Proposed repair · reviewed "
        } else {
            " Proposed repair "
        },
        plan,
        Color::Yellow,
    );
}

fn render_panel(frame: &mut Frame<'_>, area: Rect, title: &str, text: String, color: Color) {
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let footer = match app.state {
        UiState::ConfirmAnalysis {
            replace_saved: false,
        } => "Send scrubbed evidence to AI?  [y] Yes  [n] No".to_string(),
        UiState::ConfirmAnalysis {
            replace_saved: true,
        } => "Replace saved analysis?  [y] Re-analyze  [n] Keep saved".to_string(),
        UiState::ConfirmRepair => {
            "Apply this reviewed repair, verify it, and roll back on failure?  [y] Apply  [n] Cancel"
                .to_string()
        }
        UiState::ConfirmQuit => {
            "Disconnect? The background watcher stays active.  [y] Exit  [n] Stay".to_string()
        }
        UiState::Analyzing { started } => progress("AI is analyzing bounded evidence", started),
        UiState::Repairing { started } => progress("Applying, verifying, and protecting rollback", started),
        UiState::Gathering { started } => progress("Reproducing the failure and gathering evidence", started),
        _ => ready_footer(app, area.width),
    };
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn ready_footer(app: &App, width: u16) -> String {
    let evidence_action = if app.show_details {
        "[Enter] Collapse evidence"
    } else {
        "[Enter] Expand evidence"
    };
    let analysis_action = if app.analysis.is_some() {
        ""
    } else {
        "[A] Analyze"
    };
    let repair_action = if app
        .analysis
        .as_ref()
        .is_some_and(|value| !value.proposed_actions.is_empty())
    {
        if app.show_repair {
            "[R] Request approval"
        } else {
            "[R] Review plan"
        }
    } else if app
        .analysis
        .as_ref()
        .is_some_and(|value| value.needs_more_evidence && value.proposed_actions.is_empty())
    {
        "[G] Gather evidence"
    } else {
        ""
    };
    let history = if app.show_history {
        "[H] Active incidents"
    } else {
        "[H] History"
    };
    let refresh = if app.analysis.is_some() {
        "[U] Re-analyze"
    } else {
        ""
    };
    let first = format!(
        " {:<17}{:<29}{:<16}{:<20}{}",
        "[↑↓] Select", evidence_action, analysis_action, refresh, repair_action
    );
    let second = format!(" {:<17}{:<21}{}", "[D] Dismiss", history, "[Q] Disconnect");
    if width >= 170 {
        format!("{first}  {second}")
    } else {
        format!(
            " {:<17}{}\n {:<21}{:<20}{}\n{}",
            "[↑↓] Select", evidence_action, analysis_action, refresh, repair_action, second
        )
    }
}

fn progress(label: &str, started: std::time::Instant) -> String {
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let index = (started.elapsed().as_millis() / 100) as usize % frames.len();
    format!(
        " {} {}… {:.1}s ",
        frames[index],
        label,
        started.elapsed().as_secs_f32()
    )
}

fn evidence_text(incident: &Incident, expanded: bool) -> String {
    let mut lines = vec![
        format!("{}", incident.message),
        String::new(),
        format!("Source       {}", incident_source_label(incident)),
        format!("Failure      {:?}", incident.kind),
        format!("Status       {:?}", incident.status),
        format!("Confidence   {:?}", incident.confidence),
        format!("Occurrences  {}", incident.occurrence_count),
        format!(
            "Last seen    {}",
            local_timestamp(incident.last_observed_at.unwrap_or(incident.observed_at))
        ),
    ];
    for evidence in &incident.evidence {
        lines.push(String::new());
        lines.push(format!("• {}", evidence.summary));
        if expanded {
            for (key, value) in &evidence.fields {
                lines.push(format!("  {key}: {}", compact_value(value)));
            }
        }
    }
    lines.join("\n")
}

fn action_label(action_type: &str) -> String {
    match action_type {
        "quarantine_path" => "Quarantine the evidence-bound path".into(),
        "regenerate_cache" => "Regenerate the evidence-bound cache".into(),
        "patch_json_config" => "Apply the reviewed configuration change".into(),
        "set_permission" => "Restore the reviewed permissions".into(),
        "restart_service" => "Restart the exact evidence-bound service".into(),
        "restart_container" => "Restart the exact evidence-bound container".into(),
        other => other.replace('_', " "),
    }
}

fn compact_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    };
    const MAX_CHARS: usize = 180;
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut bounded = text.chars().take(MAX_CHARS).collect::<String>();
    bounded.push('…');
    bounded
}

fn incident_source_label(incident: &Incident) -> String {
    if let Some(engine) = incident
        .evidence
        .iter()
        .find_map(|evidence| evidence.fields.get("engine").and_then(Value::as_str))
    {
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

#[cfg(test)]
mod tests {
    use super::{action_label, compact_value};
    use crate::tui::{App, UiState, draw};
    use ratatui::{Terminal, backend::TestBackend};
    use rescueloop_core::{AnalysisResponse, Evidence, Hypothesis, Incident, ProposedAction};
    use serde_json::json;
    use std::{collections::BTreeMap, path::PathBuf};

    #[test]
    fn labels_typed_actions_without_exposing_a_shell_shape() {
        assert_eq!(
            action_label("restart_container"),
            "Restart the exact evidence-bound container"
        );
        assert_eq!(
            action_label("patch_json_config"),
            "Apply the reviewed configuration change"
        );
    }

    #[test]
    fn bounds_rendered_parameter_values() {
        let rendered = compact_value(&json!("x".repeat(300)));
        assert_eq!(rendered.chars().count(), 181);
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn renders_incidents_and_reviewed_plan_at_wide_and_narrow_sizes() {
        let mut incident = Incident::detected(
            "test",
            rescueloop_core::IncidentKind::RestartLoop,
            "Container restarted repeatedly",
            Evidence {
                source: "container-event".into(),
                summary: "Docker reported an out-of-memory exit".into(),
                artifact: None,
                fields: BTreeMap::from([
                    ("engine".into(), json!("docker")),
                    ("exit_code".into(), json!(137)),
                ]),
            },
        );
        incident.application = Some("checkout-api".into());
        let app = App {
            incidents: vec![(incident, PathBuf::from("incident.json"))],
            selected: 0,
            show_details: true,
            show_repair: true,
            state: UiState::Ready,
            analysis: Some(AnalysisResponse {
                summary: "The container exceeded its memory limit.".into(),
                hypotheses: vec![Hypothesis {
                    cause: "memory limit".into(),
                    confidence: 0.9,
                    evidence_indexes: vec![0],
                }],
                proposed_actions: vec![ProposedAction {
                    action_type: "restart_container".into(),
                    reason: "Restart the exact unhealthy container.".into(),
                    parameters: json!({"engine": "docker", "container_id": "fixture"}),
                    reversible: true,
                }],
                needs_more_evidence: false,
            }),
            agent_name: "fixture-agent".into(),
            show_history: false,
        };

        for (width, height) in [(180, 48), (96, 32)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Active incidents"));
            assert!(rendered.contains("Evidence"));
            assert!(rendered.contains("Analysis · saved"));
            assert!(rendered.contains("Proposed repair"));
            assert!(rendered.contains("Request approval"));
            assert!(rendered.contains("Collapse evidence"));
            assert!(!rendered.contains("[A] Saved"));
        }
    }
}
