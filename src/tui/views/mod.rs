use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::status_glyph;
use crate::app::state::{AppState, View};
use crate::provider::{Run, Status};

pub fn render(f: &mut Frame, state: &AppState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    render_header(f, chunks[0], state);
    match state.view {
        View::Workflows => render_workflows(f, chunks[1], state),
        View::Runs => render_runs(f, chunks[1], state),
        View::RunDetail => render_run_detail(f, chunks[1], state),
        View::Logs => render_logs(f, chunks[1], state),
        View::Watch => render_watch(f, chunks[1], state),
        View::TriggerPrompt => render_trigger_prompt(f, chunks[1], state),
    }
    render_footer(f, chunks[2], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let view_name = format!("{:?}", state.view);
    let badge = Span::styled(" ⚡ jog ", Style::default().fg(Color::White).bg(Color::Blue).bold());
    let sep = Span::raw("  ");
    let repo = Span::styled(state.repo_label.clone(), Style::default().fg(Color::White).bold());
    let branch = Span::styled(
        format!("  [{}]", state.current_branch),
        Style::default().fg(Color::Yellow),
    );
    let pad = Span::raw("  ");
    let view_label = Span::styled(view_name, Style::default().fg(Color::Magenta).bold());
    let line = Line::from(vec![badge, sep, repo, branch, pad, view_label]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let editing = state
        .trigger_prompt
        .as_ref()
        .map(|p| p.editing)
        .unwrap_or(false);
    let hints: &[(&str, &str)] = match state.view {
        View::Workflows => &[
            ("↵", "runs"),
            ("t", "trigger"),
            ("w", "watch"),
            ("o", "open"),
            ("q", "quit"),
        ],
        View::Runs => &[
            ("↵", "detail"),
            ("t", "trigger"),
            ("r", "rerun"),
            ("R", "rerun-failed"),
            ("x", "cancel"),
            ("w", "watch"),
            ("Esc", "back"),
        ],
        View::RunDetail => &[("↵/l", "logs"), ("Esc", "back"), ("q", "quit")],
        View::Logs => &[
            ("j/k", "scroll"),
            ("d/u", "page"),
            ("g", "top"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        View::Watch => &[("Esc", "back"), ("q", "quit")],
        View::TriggerPrompt if editing => &[
            ("type", "edit"),
            ("Bksp", "delete"),
            ("Enter/Esc", "done"),
        ],
        View::TriggerPrompt => &[
            ("j/k", "move"),
            ("Space", "cycle"),
            ("↵/i", "edit"),
            ("t", "submit"),
            ("Esc", "cancel"),
        ],
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(*key, Style::default().fg(Color::White).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(Color::DarkGray)));
    }

    if state.pending > 0 {
        spans.push(Span::styled("  ⟳", Style::default().fg(Color::Yellow)));
    }

    if let Some(msg) = &state.status_msg {
        spans.push(Span::styled("   │   ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(msg.clone(), Style::default().fg(Color::White)));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn styled_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Cyan).bold(),
        ))
}

fn render_workflows(f: &mut Frame, area: Rect, state: &AppState) {
    let count = state.workflows.len();
    let title = format!("Workflows ({count})");
    let blk = styled_block(&title);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height == 0 {
        return;
    }

    let header_area = Rect { height: 1, ..inner };
    let list_area = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };

    let hdr_style = Style::default().fg(Color::Rgb(140, 140, 160)).add_modifier(Modifier::UNDERLINED);
    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(format!("{:<32}", "Workflow"), hdr_style),
        Span::styled(format!("{:<28}", "File"), hdr_style),
        Span::styled("Last run", hdr_style),
    ]);
    f.render_widget(Paragraph::new(header), header_area);

    let items: Vec<ListItem> = state
        .workflows
        .iter()
        .map(|w| {
            let status = w.last_status.unwrap_or(Status::Unknown);
            let glyph = status_glyph(status);
            let glyph_style = style_for_status(status);
            let trig = if w.triggerable { "[t]" } else { "   " };
            let (when, when_style) = w
                .last_run_at
                .map(|t| relative_styled(t.with_timezone(&Utc)))
                .unwrap_or_else(|| ("—".into(), Style::default().fg(Color::DarkGray)));
            ListItem::new(Line::from(vec![
                Span::styled(glyph, glyph_style),
                Span::raw(" "),
                Span::raw(pad_dw(&truncate_dw(&w.name, 32), 32)),
                Span::styled(
                    pad_dw(&truncate_dw(&w.file_name, 28), 28),
                    Style::default().fg(Color::Rgb(140, 140, 160)),
                ),
                Span::styled(format!("{:<10}", when), when_style),
                Span::styled(trig, Style::default().fg(Color::Yellow).bold()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(0, 206, 209))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut s = ListState::default();
    if !state.workflows.is_empty() {
        s.select(Some(state.workflow_cursor));
    }
    f.render_stateful_widget(list, list_area, &mut s);
}

fn render_runs(f: &mut Frame, area: Rect, state: &AppState) {
    let title = match &state.workflow_for_runs {
        Some(f) => format!("Runs — {} ({})", f, state.runs.len()),
        None => format!("Runs ({})", state.runs.len()),
    };
    let blk = styled_block(&title);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height == 0 {
        return;
    }

    let header_area = Rect { height: 1, ..inner };
    let list_area = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };

    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(format!("{:<10}", "ID"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<24}", "Branch"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<40}", "Title"), Style::default().fg(Color::DarkGray)),
        Span::styled("Updated", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(header), header_area);

    let items: Vec<ListItem> = state
        .runs
        .iter()
        .map(|r| {
            let (when, when_style) = relative_styled(r.updated_at);
            ListItem::new(Line::from(vec![
                Span::styled(status_glyph(r.status), style_for_status(r.status)),
                Span::raw(" "),
                Span::raw(format!("{:<10}", r.id)),
                Span::styled(
                    format!("{:<24}", truncate(&r.head_branch, 24)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!("{:<40}", truncate(&r.display_title, 40))),
                Span::styled(when, when_style),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(64, 224, 208))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut s = ListState::default();
    if !state.runs.is_empty() {
        s.select(Some(state.run_cursor));
    }
    f.render_stateful_widget(list, list_area, &mut s);
}

fn render_run_detail(f: &mut Frame, area: Rect, state: &AppState) {
    let detail = match &state.run_detail {
        Some(d) => d,
        None => {
            let p = Paragraph::new("loading…").block(styled_block("Run"));
            f.render_widget(p, area);
            return;
        }
    };
    let mut lines = Vec::new();
    for (i, job) in detail.jobs.iter().enumerate() {
        let prefix = if i == state.job_cursor { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::raw(prefix),
            Span::styled(status_glyph(job.status), style_for_status(job.status)),
            Span::raw(" "),
            Span::styled(
                job.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for step in &job.steps {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(status_glyph(step.status), style_for_status(step.status)),
                Span::raw(" "),
                Span::styled(
                    format!("{}. {}", step.number, step.name),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
    }
    let title = format!(
        "Run {} — {} ({})",
        detail.run.id, detail.run.display_title, detail.run.head_branch
    );
    let p = Paragraph::new(lines)
        .block(styled_block(&title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    // inner width = area minus the two border columns
    let sep = "─".repeat(area.width.saturating_sub(2) as usize);

    let body: Vec<Line> = state
        .log_lines
        .iter()
        .flat_map(|l| {
            // ##[group] is GitHub Actions' step-level section header.
            // ##[section] serves the same role in some runners.
            if let Some(title) = l.strip_prefix("##[group]").or_else(|| l.strip_prefix("##[section]")) {
                vec![
                    Line::from(Span::styled(sep.clone(), Style::default().fg(Color::DarkGray))),
                    Line::from(vec![
                        Span::styled("▸ ", Style::default().fg(Color::Cyan).bold()),
                        Span::styled(title.to_string(), Style::default().fg(Color::Cyan).bold()),
                    ]),
                    Line::default(),
                ]
            } else if l.starts_with("##[endgroup]") {
                // blank line after each section body
                vec![Line::default()]
            } else if let Some(cmd) = l.strip_prefix("##[command]") {
                vec![Line::from(vec![
                    Span::styled("  $ ", Style::default().fg(Color::Green).bold()),
                    Span::styled(cmd.to_string(), Style::default().fg(Color::White)),
                ])]
            } else if let Some(msg) = l.strip_prefix("##[error]") {
                vec![Line::from(Span::styled(
                    format!("✗ {msg}"),
                    Style::default().fg(Color::Red).bold(),
                ))]
            } else if let Some(msg) = l.strip_prefix("##[warning]") {
                vec![Line::from(Span::styled(
                    format!("⚠ {msg}"),
                    Style::default().fg(Color::Yellow).bold(),
                ))]
            } else {
                let lower = l.to_lowercase();
                let style = if lower.contains("error") || lower.contains("failed") {
                    Style::default().fg(Color::Red)
                } else if lower.contains("warn") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Rgb(200, 200, 200))
                };
                vec![Line::from(Span::styled(l.clone(), style))]
            }
        })
        .collect();

    let p = Paragraph::new(body)
        .block(styled_block("Logs"))
        .wrap(Wrap { trim: false })
        .scroll((state.log_scroll, 0));
    f.render_widget(p, area);
}

fn render_watch(f: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let summary_lines = if let Some(detail) = &state.run_detail {
        let elapsed = format_elapsed(elapsed_seconds(&detail.run));
        let step = detail.current_step().unwrap_or("—");
        vec![
            Line::from(vec![
                Span::styled("Status: ", Style::default().fg(Color::Cyan).bold()),
                Span::styled(
                    format!("{:?}", detail.run.status),
                    style_for_status(detail.run.status),
                ),
            ]),
            Line::from(vec![
                Span::styled("Step:   ", Style::default().fg(Color::Cyan).bold()),
                Span::raw(step.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Elapsed:", Style::default().fg(Color::Cyan).bold()),
                Span::raw(format!(" {}", elapsed)),
            ]),
            Line::from(vec![
                Span::styled("Run:    ", Style::default().fg(Color::Cyan).bold()),
                Span::raw(format!(
                    "{} on {}",
                    detail.run.display_title, detail.run.head_branch
                )),
            ]),
        ]
    } else if let Some(run) = state.runs.first() {
        vec![Line::from(format!("waiting on run {}…", run.id))]
    } else {
        vec![Line::from("loading runs…".to_string())]
    };

    let summary = Paragraph::new(summary_lines).block(styled_block("Watch"));
    f.render_widget(summary, chunks[0]);

    if let Some(detail) = &state.run_detail {
        let mut lines = Vec::new();
        for job in &detail.jobs {
            lines.push(Line::from(vec![
                Span::styled(status_glyph(job.status), style_for_status(job.status)),
                Span::raw(" "),
                Span::styled(job.name.clone(), Style::default().bold()),
            ]));
            for step in &job.steps {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(status_glyph(step.status), style_for_status(step.status)),
                    Span::raw(" "),
                    Span::raw(step.name.clone()),
                ]));
            }
        }
        let p = Paragraph::new(lines).block(styled_block("Jobs"));
        f.render_widget(p, chunks[1]);
    }
}

fn style_for_status(s: Status) -> Style {
    match s {
        Status::Success => Style::default().fg(Color::Green).bold(),
        Status::Failure => Style::default().fg(Color::Red).bold(),
        Status::Running => Style::default().fg(Color::Yellow).bold(),
        Status::Queued => Style::default().fg(Color::Blue).bold(),
        Status::Cancelled => Style::default().fg(Color::DarkGray),
        Status::Skipped => Style::default().fg(Color::DarkGray),
        Status::Unknown => Style::default().fg(Color::Gray),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let margin_y = (100 - percent_y) / 2;
    let margin_x = (100 - percent_x) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(margin_y),
            Constraint::Percentage(percent_y),
            Constraint::Percentage(margin_y),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(margin_x),
            Constraint::Percentage(percent_x),
            Constraint::Percentage(margin_x),
        ])
        .split(vertical[1])[1]
}

fn render_trigger_prompt(f: &mut Frame, area: Rect, state: &AppState) {
    let modal = centered_rect(70, 80, area);

    let Some(prompt) = state.trigger_prompt.as_ref() else {
        let p = Paragraph::new("(no prompt)").block(styled_block("Trigger"));
        f.render_widget(p, modal);
        return;
    };
    let title = format!(
        "Trigger: {}  ({})  on {}",
        prompt.workflow_name, prompt.workflow_file, state.current_branch
    );
    let mut lines = Vec::new();
    let name_width = prompt
        .fields
        .iter()
        .map(|f| f.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    for (i, field) in prompt.fields.iter().enumerate() {
        let selected = i == prompt.cursor;
        let prefix = if selected { "▶ " } else { "  " };
        let label_style = if selected {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let value_display = if field.value.is_empty() {
            "(empty)".to_string()
        } else {
            field.value.clone()
        };
        let editing_marker = if selected && prompt.editing { "_" } else { "" };
        let value_style = if selected && prompt.editing {
            Style::default().fg(Color::Yellow).bold()
        } else if selected {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut spans = vec![
            Span::raw(prefix),
            Span::styled(format!("{:<width$}", field.name, width = name_width), label_style),
            Span::raw("  "),
            Span::styled(value_display, value_style),
            Span::styled(editing_marker, Style::default().fg(Color::Yellow)),
        ];
        if field.required {
            spans.push(Span::styled(
                "  (required)",
                Style::default().fg(Color::Red),
            ));
        }
        if let Some(opts) = &field.options {
            spans.push(Span::styled(
                format!("  [{}]", opts.join("/")),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    if prompt.fields.is_empty() {
        lines.push(Line::from("(no inputs)"));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("t", Style::default().fg(Color::White).bold()),
        Span::styled(" trigger  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::White).bold()),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]));
    let p = Paragraph::new(lines)
        .block(styled_block(&title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, modal);
}

fn elapsed_seconds(run: &Run) -> i64 {
    // For terminal runs, freeze the clock at completion. Otherwise it would
    // keep ticking forever on a Skipped/Failed/Success run.
    let end = if run.status.is_terminal() {
        run.updated_at
    } else {
        Utc::now()
    };
    (end - run.created_at).num_seconds().max(0)
}

fn format_elapsed(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn relative_styled(t: chrono::DateTime<Utc>) -> (String, Style) {
    let secs = (Utc::now() - t).num_seconds().max(0);
    let text = if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    };
    let style = if secs < 3600 {
        Style::default().fg(Color::White)
    } else if secs < 86400 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    (text, style)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn char_display_width(c: char) -> usize {
    let cp = c as u32;
    // Emoji (most common ranges) and CJK characters render as 2 terminal columns.
    if (0x1F300..=0x1FAFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0x3000..=0x303F).contains(&cp)
    {
        2
    } else {
        1
    }
}

fn truncate_dw(s: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let cw = char_display_width(c);
        if width + cw > max_cols {
            out.push('…');
            return out;
        }
        out.push(c);
        width += cw;
    }
    out
}

fn pad_dw(s: &str, cols: usize) -> String {
    let dw: usize = s.chars().map(char_display_width).sum();
    format!("{}{}", s, " ".repeat(cols.saturating_sub(dw)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn format_elapsed_under_hour() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(42), "0:42");
        assert_eq!(format_elapsed(125), "2:05");
        assert_eq!(format_elapsed(3599), "59:59");
    }

    #[test]
    fn format_elapsed_over_hour() {
        assert_eq!(format_elapsed(3600), "1:00:00");
        assert_eq!(format_elapsed(6942), "1:55:42");
    }

    #[test]
    fn elapsed_freezes_for_terminal_runs() {
        let created = Utc.with_ymd_and_hms(2026, 4, 29, 8, 0, 0).unwrap();
        let updated = Utc.with_ymd_and_hms(2026, 4, 29, 8, 0, 5).unwrap();
        let run = Run {
            id: 1,
            display_title: "x".into(),
            head_branch: "main".into(),
            status: Status::Skipped,
            created_at: created,
            updated_at: updated,
            url: String::new(),
        };
        // Skipped 5s after creation; should stay 5s no matter when we look.
        assert_eq!(elapsed_seconds(&run), 5);
    }
}
