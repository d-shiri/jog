use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::status_glyph;
use crate::app::state::{AppState, View};
use crate::provider::Status;

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
    }
    render_footer(f, chunks[2], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let title = format!(
        " jog  {}  [{}]   view: {:?}",
        state.repo_label, state.current_branch, state.view
    );
    let para = Paragraph::new(title).style(Style::default().fg(Color::Cyan).bold());
    f.render_widget(para, area);
}

fn render_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let hint = match state.view {
        View::Workflows => "↵ runs  t trigger  w watch  o open  q quit",
        View::Runs => "↵ detail  t trigger  r rerun  R rerun-failed  x cancel  w watch  Esc back",
        View::RunDetail => "↵/l logs  Esc back  q quit",
        View::Logs => "j/k scroll  d/u page  g top  Esc back  q quit",
        View::Watch => "Esc back  q quit",
    };
    let txt = match &state.status_msg {
        Some(m) => format!(" {}   |   {}", hint, m),
        None => format!(" {}", hint),
    };
    let para = Paragraph::new(txt).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

fn render_workflows(f: &mut Frame, area: Rect, state: &AppState) {
    let items: Vec<ListItem> = state
        .workflows
        .iter()
        .map(|w| {
            let status = w.last_status.unwrap_or(Status::Unknown);
            let glyph = status_glyph(status);
            let glyph_style = style_for_status(status);
            let trig = if w.triggerable { "[t]" } else { "   " };
            let when = w
                .last_run_at
                .map(|t| relative(t.with_timezone(&Utc)))
                .unwrap_or_else(|| "—".into());
            ListItem::new(Line::from(vec![
                Span::styled(glyph, glyph_style),
                Span::raw(" "),
                Span::raw(format!("{:<32}", truncate(&w.name, 32))),
                Span::styled(
                    format!("{:<28}", w.file_name),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:<10}", when)),
                Span::styled(trig, Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Workflows "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut s = ListState::default();
    if !state.workflows.is_empty() {
        s.select(Some(state.workflow_cursor));
    }
    f.render_stateful_widget(list, area, &mut s);
}

fn render_runs(f: &mut Frame, area: Rect, state: &AppState) {
    let title = match &state.workflow_for_runs {
        Some(f) => format!(" Runs — {} ", f),
        None => " Runs ".into(),
    };
    let items: Vec<ListItem> = state
        .runs
        .iter()
        .map(|r| {
            ListItem::new(Line::from(vec![
                Span::styled(status_glyph(r.status), style_for_status(r.status)),
                Span::raw(" "),
                Span::raw(format!("{:<10}", r.id)),
                Span::raw(format!("{:<24}", truncate(&r.head_branch, 24))),
                Span::raw(format!("{:<40}", truncate(&r.display_title, 40))),
                Span::styled(relative(r.updated_at), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut s = ListState::default();
    if !state.runs.is_empty() {
        s.select(Some(state.run_cursor));
    }
    f.render_stateful_widget(list, area, &mut s);
}

fn render_run_detail(f: &mut Frame, area: Rect, state: &AppState) {
    let detail = match &state.run_detail {
        Some(d) => d,
        None => {
            let p = Paragraph::new("loading...")
                .block(Block::default().borders(Borders::ALL).title(" Run "));
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
        " Run {} — {} ({}) ",
        detail.run.id, detail.run.display_title, detail.run.head_branch
    );
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    let body: Vec<Line> = state
        .log_lines
        .iter()
        .map(|l| {
            let lower = l.to_lowercase();
            let style = if lower.contains("error") || lower.contains("failed") {
                Style::default().fg(Color::Red)
            } else if lower.contains("warn") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    let p = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title(" Logs "))
        .scroll((state.log_scroll, 0));
    f.render_widget(p, area);
}

fn render_watch(f: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let summary_lines = if let Some(detail) = &state.run_detail {
        let elapsed = (Utc::now() - detail.run.created_at).num_seconds().max(0);
        let mins = elapsed / 60;
        let secs = elapsed % 60;
        let step = detail.current_step().unwrap_or("—");
        vec![
            Line::from(vec![
                Span::styled("Status: ", Style::default().bold()),
                Span::styled(
                    format!("{:?}", detail.run.status),
                    style_for_status(detail.run.status),
                ),
            ]),
            Line::from(vec![
                Span::styled("Step:   ", Style::default().bold()),
                Span::raw(step.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Elapsed:", Style::default().bold()),
                Span::raw(format!(" {}:{:02}", mins, secs)),
            ]),
            Line::from(vec![
                Span::styled("Run:    ", Style::default().bold()),
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

    let summary = Paragraph::new(summary_lines)
        .block(Block::default().borders(Borders::ALL).title(" Watch "));
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
        let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Jobs "));
        f.render_widget(p, chunks[1]);
    }
}

fn style_for_status(s: Status) -> Style {
    match s {
        Status::Success => Style::default().fg(Color::Green),
        Status::Failure => Style::default().fg(Color::Red),
        Status::Running => Style::default().fg(Color::Yellow),
        Status::Queued => Style::default().fg(Color::Blue),
        Status::Cancelled => Style::default().fg(Color::DarkGray),
        Status::Skipped => Style::default().fg(Color::DarkGray),
        Status::Unknown => Style::default().fg(Color::Gray),
    }
}

fn relative(t: chrono::DateTime<Utc>) -> String {
    let secs = (Utc::now() - t).num_seconds().max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
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
