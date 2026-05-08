use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, LineGauge, Paragraph, Row, Table, TableState, Wrap,
};

use std::collections::HashMap;

use super::animated_glyph;
use crate::app::state::{AppState, DetailItem, Theme, View, build_detail_items};
use crate::history::HistoryEntry;
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
        View::Diff => render_diff(f, chunks[1], state),
    }
    render_footer(f, chunks[2], state);
    if state.view == View::Logs {
        render_search_overlay(f, area, state);
    }
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let sep = || Span::styled("  ›  ", Style::default().fg(Color::Rgb(55, 55, 80)));
    let crumb: Vec<Span> = match state.view {
        View::Workflows => vec![
            Span::styled("Workflows", Style::default().fg(theme.primary).bold()),
        ],
        View::Runs => vec![
            Span::styled("Workflows", Style::default().fg(theme.secondary)),
            sep(),
            Span::styled(
                state.workflow_for_runs.as_deref().unwrap_or("?").to_string(),
                Style::default().fg(theme.primary).bold(),
            ),
        ],
        View::RunDetail => {
            let wf = state.workflow_for_runs.as_deref().unwrap_or("?");
            let rid = state.run_detail.as_ref()
                .map(|d| format!("#{}", d.run.id))
                .unwrap_or_else(|| state.runs.get(state.run_cursor)
                    .map(|r| format!("#{}", r.id))
                    .unwrap_or_else(|| "?".into()));
            vec![
                Span::styled("Workflows", Style::default().fg(theme.secondary)),
                sep(),
                Span::styled(wf.to_string(), Style::default().fg(theme.secondary)),
                sep(),
                Span::styled(rid, Style::default().fg(theme.primary).bold()),
            ]
        }
        View::Logs => {
            let step = state.current_step_idx()
                .and_then(|i| state.log_step_names.get(i))
                .or_else(|| state.log_section_idx.and_then(|i| state.log_sections.get(i)))
                .map(|s| s.as_str())
                .unwrap_or("all steps");
            vec![
                Span::styled("Logs", Style::default().fg(theme.secondary)),
                sep(),
                Span::styled(step.to_string(), Style::default().fg(theme.primary).bold()),
            ]
        }
        View::Watch         => vec![Span::styled("Watch",   Style::default().fg(theme.primary).bold())],
        View::Diff          => vec![Span::styled("Diff",    Style::default().fg(theme.primary).bold())],
        View::TriggerPrompt => vec![Span::styled("Trigger", Style::default().fg(theme.primary).bold())],
    };

    let dot = Style::default().fg(Color::Rgb(55, 55, 80));
    let mut spans = vec![
        Span::styled(" ⚡ ", Style::default().fg(theme.accent)),
        Span::styled("jog", Style::default().fg(Color::White).bold()),
        Span::styled("  ·  ", dot),
        Span::styled(state.repo_label.as_str(), Style::default().fg(Color::White)),
        Span::styled("  ⎇ ", Style::default().fg(Color::Rgb(90, 110, 150))),
        Span::styled(state.current_branch.as_str(), Style::default().fg(theme.accent)),
        Span::styled("  ·  ", dot),
    ];
    spans.extend(crumb);
    let line = Line::from(spans);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.header_bg)),
        area,
    );
}

fn display_key(s: &str) -> &str {
    match s {
        "Enter" => "↵",
        "Esc" | "Escape" => "Esc",
        "Space" => "␣",
        other => other,
    }
}

fn render_footer(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let km = &state.keymap;
    let editing = state.trigger_prompt.as_ref().map(|p| p.editing).unwrap_or(false);

    let hints: Vec<(String, &str)> = match state.view {
        View::Workflows => vec![
            ("↵".into(), "runs"),
            (display_key(&km.trigger).into(), "trigger"),
            (display_key(&km.watch).into(), "watch"),
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::Runs => vec![
            ("↵".into(), "detail"),
            (display_key(&km.trigger).into(), "trigger"),
            (display_key(&km.rerun).into(), "rerun"),
            (display_key(&km.rerun_failed).into(), "rerun-failed"),
            (display_key(&km.cancel_run).into(), "cancel"),
            (display_key(&km.watch).into(), "watch"),
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.back).into(), "back"),
        ],
        View::RunDetail => vec![
            (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "step"),
            (format!("↵/{}", display_key(&km.open_logs)), "logs"),
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.diff).into(), "diff"),
            (display_key(&km.back).into(), "back"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::Logs => {
            let np_label = if state.log_search_query.is_some() { "match" } else { "step" };
            let mut hints = vec![
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "move"),
                (format!("{}/{}", display_key(&km.page_down), display_key(&km.page_up)), "page"),
                (format!("{}/{}", display_key(&km.next_step), display_key(&km.prev_step)), np_label),
                (display_key(&km.all_steps).into(), "all"),
                (display_key(&km.search).into(), "search"),
            ];
            if !state.log_groups.is_empty() {
                hints.push(("↵".into(), "expand/collapse"));
            }
            hints.push((display_key(&km.open_browser).into(), "open"));
            hints.push((display_key(&km.back).into(), "back"));
            hints.push((display_key(&km.quit).into(), "quit"));
            hints
        },
        View::Watch => vec![
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.back).into(), "back"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::Diff => vec![
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.back).into(), "back"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::TriggerPrompt if editing => vec![
            ("type".into(), "edit"),
            ("Bksp".into(), "delete"),
            ("Enter/Esc".into(), "done"),
        ],
        View::TriggerPrompt => vec![
            (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "move"),
            (display_key(&km.tp_cycle).into(), "cycle"),
            (format!("↵/{}", display_key(&km.tp_edit)), "edit"),
            (display_key(&km.tp_submit).into(), "trigger"),
            (display_key(&km.back).into(), "cancel"),
        ],
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(key.clone(), Style::default().fg(Color::White).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(theme.secondary)));
    }

    if state.pending > 0 {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = frames[(state.tick_count % frames.len() as u64) as usize];
        spans.push(Span::styled(format!("  {frame}"), Style::default().fg(theme.accent)));
    }

    if let Some(msg) = &state.status_msg {
        spans.push(Span::styled("   │   ", Style::default().fg(Color::Rgb(55, 55, 80))));
        spans.push(Span::styled(msg.clone(), Style::default().fg(Color::White)));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.footer_bg)),
        area,
    );
}

fn styled_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(55, 55, 80)))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(theme.primary).bold(),
        ))
}

fn render_workflows(f: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    render_workflows_list(f, chunks[0], state);
    render_workflows_preview(f, chunks[1], state);
}

fn render_workflows_list(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let count = state.workflows.len();
    let (wf_ok, wf_fail, wf_run) = state.workflows.iter().fold((0u32, 0u32, 0u32), |(o, f, r), w| {
        match w.last_status.unwrap_or(Status::Unknown) {
            Status::Success => (o + 1, f, r),
            Status::Failure => (o, f + 1, r),
            Status::Running => (o, f, r + 1),
            _ => (o, f, r),
        }
    });
    let wf_title = Line::from(vec![
        Span::styled(format!(" Workflows ({count}"), Style::default().fg(theme.primary).bold()),
        Span::styled(format!("  ✓{wf_ok}"), Style::default().fg(theme.success)),
        Span::styled(format!("  ✗{wf_fail}"), Style::default().fg(theme.failure)),
        if wf_run > 0 { Span::styled(format!("  ⏵{wf_run}"), Style::default().fg(theme.warning).bold()) } else { Span::raw("") },
        Span::styled(" ) ", Style::default().fg(theme.primary).bold()),
    ]);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(55, 55, 80)))
        .title(wf_title);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    let sel_bg = Color::Rgb(35, 95, 120);
    let sel_fg = Color::Rgb(220, 240, 255);
    let hdr = Style::default().fg(theme.secondary);

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled("Workflow", hdr)),
        Cell::from(Span::styled("File", hdr)),
        Cell::from(Span::styled("Last run", hdr)),
        Cell::from(""),
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = state
        .workflows
        .iter()
        .map(|w| {
            let status = w.last_status.unwrap_or(Status::Unknown);
            let (when_text, when_style) = w
                .last_run_at
                .map(|t| relative_styled(t.with_timezone(&Utc)))
                .unwrap_or_else(|| ("—".into(), Style::default().fg(theme.unknown)));
            let trig = if w.triggerable { "t" } else { " " };

            let row_bg = row_bg_for_status(status);
            Row::new(vec![
                Cell::from(Span::styled(animated_glyph(status, state.tick_count), style_for_status(status, &state.theme))),
                Cell::from(Span::styled(w.name.clone(), Style::default())),
                Cell::from(Span::styled(
                    w.file_name.clone(),
                    Style::default().fg(Color::Rgb(110, 110, 140)),
                )),
                Cell::from(Span::styled(when_text, when_style)),
                Cell::from(Span::styled(
                    trig,
                    Style::default().fg(theme.accent),
                )),
            ])
            .style(Style::default().bg(row_bg))
        })
        .collect();

    let widths = [
        Constraint::Length(1),       // status glyph
        Constraint::Fill(55),        // workflow name
        Constraint::Fill(45),        // file
        Constraint::Length(10),      // last run
        Constraint::Length(1),       // trig
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::default().bg(sel_bg).fg(sel_fg).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut ts = TableState::default();
    if !state.workflows.is_empty() {
        ts.select(Some(state.workflow_cursor));
    }
    f.render_stateful_widget(table, inner, &mut ts);
}

fn render_workflows_preview(f: &mut Frame, area: Rect, state: &AppState) {
    let selected = state.workflows.get(state.workflow_cursor);

    let title = selected
        .map(|w| w.name.clone())
        .unwrap_or_else(|| "Runs".into());

    let blk = styled_block(&title, &state.theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    let preview_ready = selected
        .map(|w| state.workflow_preview_file.as_deref() == Some(w.file_name.as_str()))
        .unwrap_or(false)
        && !state.workflow_preview_runs.is_empty();

    if !preview_ready {
        f.render_widget(
            Paragraph::new(Span::styled("loading…", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    let theme = &state.theme;

    // Split inner: top 3 rows for sparkline trend, rest for the runs table
    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);

    let bar_area = inner_chunks[0];
    let bar_w: u16 = 3;
    let gap: u16 = 1;
    let step = bar_w + gap;
    let max_bars = ((bar_area.width + gap) / step) as usize;
    for (i, run) in state.workflow_preview_runs.iter().rev().take(max_bars).enumerate() {
        let bar_color = match run.status {
            Status::Success                     => theme.success,
            Status::Failure                     => theme.failure,
            Status::Running                     => theme.warning,
            Status::Cancelled | Status::Skipped => theme.unknown,
            _                                   => theme.unknown,
        };
        let x = bar_area.x + i as u16 * step;
        if x + bar_w > bar_area.x + bar_area.width { break; }
        let h = (bar_area.height / 2).max(1);
        let y = bar_area.y + (bar_area.height - h) / 2;
        let rect = Rect { x, y, width: bar_w, height: h };
        f.render_widget(Block::default().style(Style::default().bg(bar_color)), rect);
    }

    let sel_bg = Color::Rgb(25, 85, 110);
    let sel_fg = Color::Rgb(220, 240, 255);
    let hdr = Style::default().fg(Color::Rgb(120, 120, 145));

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled("Branch", hdr)),
        Cell::from(Span::styled("Updated", hdr)),
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = state
        .workflow_preview_runs
        .iter()
        .map(|r| {
            let (when_text, when_style) = relative_styled(r.updated_at);
            Row::new(vec![
                Cell::from(Span::styled(animated_glyph(r.status, state.tick_count), style_for_status(r.status, &state.theme))),
                Cell::from(Span::styled(r.head_branch.clone(), Style::default().fg(Color::Yellow))),
                Cell::from(Span::styled(when_text, when_style)),
            ])
            .style(Style::default().bg(row_bg_for_status(r.status)))
        })
        .collect();

    let widths = [
        Constraint::Length(1),       // status glyph
        Constraint::Fill(1),         // branch (fills remaining)
        Constraint::Length(10),      // updated
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::default().bg(sel_bg).fg(sel_fg).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut ts = TableState::default();
    if !state.workflow_preview_runs.is_empty() {
        ts.select(Some(0)); // highlight most recent
    }
    f.render_stateful_widget(table, inner_chunks[1], &mut ts);
}

fn render_runs(f: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    render_runs_list(f, chunks[0], state);
    render_runs_preview(f, chunks[1], state);
}

fn render_runs_list(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let wf_label = state.workflow_for_runs.as_deref().unwrap_or("?");
    let (ok, fail, running) = state.runs.iter().fold((0u32, 0u32, 0u32), |(o, f, r), x| {
        match x.status {
            Status::Success => (o + 1, f, r),
            Status::Failure => (o, f + 1, r),
            Status::Running => (o, f, r + 1),
            _ => (o, f, r),
        }
    });
    let title_line = Line::from(vec![
        Span::styled(
            if state.workflow_for_runs.is_some() {
                format!(" Runs — {} ({}", wf_label, state.runs.len())
            } else {
                format!(" Runs ({}", state.runs.len())
            },
            Style::default().fg(theme.primary).bold(),
        ),
        Span::styled(format!("  ✓{ok}"), Style::default().fg(theme.success)),
        Span::styled(format!("  ✗{fail}"), Style::default().fg(theme.failure)),
        if running > 0 { Span::styled(format!("  ⏵{running}"), Style::default().fg(theme.warning).bold()) } else { Span::raw("") },
        Span::styled(" ) ", Style::default().fg(theme.primary).bold()),
    ]);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(55, 55, 80)))
        .style(Style::default().bg(Color::Rgb(18, 20, 32)))
        .title(title_line);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    let sel_bg = Color::Rgb(25, 85, 110);
    let sel_fg = Color::Rgb(220, 240, 255);
    let hdr = Style::default().fg(Color::Rgb(120, 120, 145));

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled("Branch", hdr)),
        Cell::from(Span::styled("Updated", hdr)),
        Cell::from(Span::styled("Dur", hdr)),
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = state
        .runs
        .iter()
        .map(|r| {
            let (when_text, when_style) = relative_styled(r.updated_at);
            let dur_secs = elapsed_seconds(r);
            let dur_text = format_elapsed(dur_secs);
            let dur_style = if dur_secs > 900 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Rgb(110, 110, 140))
            };
            let commit_line = if r.commit_msg.is_empty() {
                Line::default()
            } else {
                const MAX: usize = 52;
                let chars: Vec<char> = r.commit_msg.chars().collect();
                let msg = if chars.len() > MAX {
                    format!("{}…", chars[..MAX].iter().collect::<String>())
                } else {
                    chars.iter().collect()
                };
                Line::from(vec![
                    Span::styled("⎿ ", Style::default().fg(Color::Rgb(65, 65, 80))),
                    Span::styled(msg, Style::default().fg(Color::Rgb(110, 110, 140))),
                ])
            };
            let branch_cell = ratatui::text::Text::from(vec![
                Line::from(Span::styled(r.head_branch.clone(), Style::default().fg(Color::Yellow))),
                commit_line,
            ]);
            Row::new(vec![
                Cell::from(Span::styled(animated_glyph(r.status, state.tick_count), style_for_status(r.status, &state.theme))),
                Cell::from(branch_cell),
                Cell::from(Span::styled(when_text, when_style)),
                Cell::from(Span::styled(dur_text, dur_style)),
            ])
            .height(2)
        })
        .collect();

    let widths = [
        Constraint::Length(1),   // glyph
        Constraint::Fill(1),     // branch + commit
        Constraint::Length(10),  // updated
        Constraint::Length(7),   // duration
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::default().bg(sel_bg).fg(sel_fg).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut ts = TableState::default();
    if !state.runs.is_empty() {
        ts.select(Some(state.run_cursor));
    }
    f.render_stateful_widget(table, inner, &mut ts);
}

fn render_runs_preview(f: &mut Frame, area: Rect, state: &AppState) {
    let selected = state.runs.get(state.run_cursor);

    let title = selected.map(|r| {
        format!("{} — {}", r.display_title, r.head_branch)
    }).unwrap_or_else(|| "Preview".into());

    let blk = styled_block(&title, &state.theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height == 0 {
        return;
    }

    let preview_ready = selected
        .map(|r| state.runs_preview_id == Some(r.id))
        .unwrap_or(false)
        && state.runs_preview.is_some();

    if !preview_ready {
        f.render_widget(
            Paragraph::new(Span::styled("loading…", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    if let Some(detail) = &state.runs_preview {
        let mut lines: Vec<Line> = Vec::new();

        if let Some(run) = selected
            && !run.commit_msg.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("󰊢 ", Style::default().fg(Color::Rgb(120, 120, 145))),
                    Span::styled(run.commit_msg.clone(), Style::default().fg(Color::Rgb(180, 180, 210)).italic()),
                ]));
                lines.push(Line::default());
            }

        for job in &detail.jobs {
            lines.push(Line::from(vec![
                Span::styled(animated_glyph(job.status, state.tick_count), style_for_status(job.status, &state.theme)),
                Span::raw(" "),
                Span::styled(job.name.clone(), Style::default().bold()),
            ]));
            for (si, step) in job.steps.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(animated_glyph(step.status, state.tick_count), style_for_status(step.status, &state.theme)),
                    Span::raw(format!(" {}. {}", si + 1, step.name)),
                ]));
            }
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn render_run_detail(f: &mut Frame, area: Rect, state: &AppState) {
    let detail = match &state.run_detail {
        Some(d) => d,
        None => {
            let p = Paragraph::new("loading…").block(styled_block("Run", &state.theme));
            f.render_widget(p, area);
            return;
        }
    };

    let stats: HashMap<String, (u32, u32)> = state
        .workflow_for_runs
        .as_deref()
        .map(|wf| state.history.step_failure_stats(wf, 10))
        .unwrap_or_default();

    // Max completed step duration per job — used to scale the ■ bars.
    let max_secs_per_job: Vec<f64> = detail.jobs.iter().map(|job| {
        job.steps.iter().filter_map(|s| {
            let ms = (s.completed_at? - s.started_at?).num_milliseconds();
            if ms > 0 { Some(ms as f64 / 1000.0) } else { None }
        }).fold(0.0_f64, f64::max)
    }).collect();

    let items = build_detail_items(detail);
    let cursor = state.detail_cursor;
    let sel_bg = Color::Rgb(28, 38, 58);

    let rows: Vec<Row> = items.iter().enumerate().map(|(flat_idx, item)| {
        let selected = flat_idx == cursor;
        let row_style = if selected { Style::default().bg(sel_bg) } else { Style::default() };
        match item {
            DetailItem::Job(ji) => {
                let job = &detail.jobs[*ji];
                let prefix = if selected { "▶ " } else { "  " };
                let name_style = if selected {
                    Style::default().bold().fg(Color::Cyan)
                } else {
                    Style::default().bold()
                };
                let name_cell = Cell::from(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(animated_glyph(job.status, state.tick_count), style_for_status(job.status, &state.theme)),
                    Span::raw(" "),
                    Span::styled(job.name.clone(), name_style),
                ]));
                Row::new(vec![name_cell, Cell::from(""), Cell::from(""), Cell::from("")])
                    .style(row_style)
            }
            DetailItem::Step { job: ji, step: si } => {
                let step = &detail.jobs[*ji].steps[*si];
                let prefix = if selected { "  ▶ " } else { "    " };
                let name_style = if selected {
                    Style::default().fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                let name_cell = Cell::from(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(animated_glyph(step.status, state.tick_count), style_for_status(step.status, &state.theme)),
                    Span::raw(format!(" {:2}. ", si + 1)),
                    Span::styled(step.name.clone(), name_style),
                ]));
                let (dur_cell, bar_cell) = if let Some((dur, bar)) =
                    step_timing(step, max_secs_per_job[*ji], state.tick_count)
                {
                    let bar_color = style_for_status(step.status, &state.theme)
                        .fg.unwrap_or(Color::DarkGray);
                    (
                        Cell::from(format!("{dur:>6}")).style(Style::default().fg(Color::Rgb(80, 80, 100))),
                        Cell::from(bar).style(Style::default().fg(bar_color)),
                    )
                } else {
                    (Cell::from(""), Cell::from(""))
                };
                let badge_cell = if let Some((failed, total)) = stats.get(&step.name).copied()
                    && failed > 0
                {
                    let s = if failed * 2 >= total {
                        Style::default().fg(Color::Red).bold()
                    } else {
                        Style::default().fg(Color::Yellow)
                    };
                    Cell::from(format!("  {failed}/{total} fails")).style(s)
                } else {
                    Cell::from("")
                };
                Row::new(vec![name_cell, dur_cell, bar_cell, badge_cell])
                    .style(row_style)
            }
        }
    }).collect();

    let title = format!(
        "Run {} — {} ({})",
        detail.run.id, detail.run.display_title, detail.run.head_branch
    );
    let blk = styled_block(&title, &state.theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    let total_jobs = detail.jobs.len();
    let failed_jobs = detail.jobs.iter().filter(|j| j.status == Status::Failure).count();
    let dur = format_elapsed(elapsed_seconds(&detail.run));
    let theme = &state.theme;
    let summary = Line::from(vec![
        Span::styled(
            format!("{total_jobs} job{}", if total_jobs == 1 { "" } else { "s" }),
            Style::default().fg(theme.primary),
        ),
        if failed_jobs > 0 {
            Span::styled(format!("  ✗ {failed_jobs} failed"), Style::default().fg(theme.failure).bold())
        } else {
            Span::styled("  ✓ all passed", Style::default().fg(theme.success))
        },
        Span::styled(format!("  ⏱ {dur}"), Style::default().fg(theme.secondary)),
    ]);
    f.render_widget(Paragraph::new(summary), inner_chunks[0]);

    let table = Table::new(rows, [
        Constraint::Min(20),     // name
        Constraint::Length(6),   // duration (right-aligned inside cell)
        Constraint::Length(10),  // ■ bar
        Constraint::Fill(1),     // historical badge
    ])
    .column_spacing(1);
    f.render_widget(table, inner_chunks[1]);
}

fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    // Inner area = area minus the rounded border (1 row top + 1 row bottom).
    let viewport = area.height.saturating_sub(2);
    state.last_logs_viewport_height.set(viewport);

    let mut log_title = if state.log_section_idx.is_some() {
        if let Some(step_idx) = state.current_step_idx() {
            let name = state.log_step_names.get(step_idx).map(|s| s.as_str()).unwrap_or("?");
            let total = state.log_step_names.len();
            format!("Logs — {name}  [{}/{}]", step_idx + 1, total)
        } else if let Some(idx) = state.log_section_idx {
            let name = state.log_sections.get(idx).map(|s| s.as_str()).unwrap_or("?");
            let total = state.log_sections.len();
            format!("Logs — {name}  [{}/{}]", idx + 1, total)
        } else {
            "Logs".to_string()
        }
    } else {
        "Logs".to_string()
    };

    if let Some(q) = &state.log_search_query {
        if state.log_search_matches.is_empty() {
            log_title.push_str(&format!("   /{q}  (no match)"));
        } else {
            let pos = state.log_search_match_idx.map(|i| i + 1).unwrap_or(0);
            log_title.push_str(&format!(
                "   /{q}  [{}/{}]",
                pos,
                state.log_search_matches.len()
            ));
        }
    }

    log_title.push_str(&format!("  ({} lines)", state.log_lines.len()));

    let p = Paragraph::new(state.log_rendered.clone())
        .block(styled_block(&log_title, &state.theme))
        .wrap(Wrap { trim: false })
        .scroll((state.log_scroll, 0));
    f.render_widget(p, area);
}

fn render_search_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    let Some(buf) = &state.log_search_input else { return };

    let dialog_w = (area.width * 60 / 100).max(40).min(area.width);
    let dialog_h = 3u16;
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup_area = Rect { x, y, width: dialog_w, height: dialog_h };

    let accent = state.theme.accent;
    let block = Block::default()
        .title(Span::styled(" Search ", Style::default().fg(accent).bold()))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));

    let inner = block.inner(popup_area);
    let content = Line::from(vec![
        Span::styled("  ", Style::default().fg(accent)),
        Span::styled(buf.as_str(), Style::default().fg(Color::White)),
        Span::styled("█", Style::default().fg(accent)),
    ]);

    f.render_widget(Clear, popup_area);
    f.render_widget(block, popup_area);
    f.render_widget(Paragraph::new(content), inner);
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
                    style_for_status(detail.run.status, &state.theme),
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

    let summary = Paragraph::new(summary_lines).block(styled_block("Watch", &state.theme));
    f.render_widget(summary, chunks[0]);

    if let Some(detail) = &state.run_detail {
        let theme = &state.theme;

        // Build alternating constraints: 1 row for the job gauge, N rows for its steps.
        let constraints: Vec<Constraint> = detail.jobs.iter()
            .flat_map(|job| [
                Constraint::Length(1),
                Constraint::Length(job.steps.len() as u16),
            ])
            .collect();

        if constraints.is_empty() {
            return;
        }

        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(chunks[1]);

        for (i, job) in detail.jobs.iter().enumerate() {
            let total = job.steps.len().max(1) as f64;
            let done = job.steps.iter().filter(|s| s.status.is_terminal()).count() as f64;
            let ratio = (done / total).clamp(0.0, 1.0);
            let g_style = style_for_status(job.status, theme);

            // ── Gauge row ──────────────────────────────────────────────
            let label = Line::from(vec![
                Span::styled(animated_glyph(job.status, state.tick_count), g_style),
                Span::raw(" "),
                Span::styled(job.name.clone(), Style::default().fg(Color::White).bold()),
                Span::styled(
                    format!("  {}/{}", done as u32, total as u32),
                    Style::default().fg(theme.secondary),
                ),
            ]);
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .label(label)
                    .filled_style(g_style)
                    .unfilled_style(Style::default().fg(Color::Rgb(55, 55, 80))),
                areas[i * 2],
            );

            // ── Steps list ─────────────────────────────────────────────
            if job.steps.is_empty() {
                continue;
            }
            let step_lines: Vec<Line> = job.steps.iter().enumerate().map(|(si, step)| {
                let (glyph_style, name_style) = match step.status {
                    Status::Success =>  (
                        Style::default().fg(theme.success),
                        Style::default().fg(Color::Rgb(100, 110, 100)),
                    ),
                    Status::Failure =>  (
                        Style::default().fg(theme.failure).bold(),
                        Style::default().fg(Color::Rgb(200, 120, 120)).bold(),
                    ),
                    Status::Running =>  (
                        style_for_status(step.status, theme).bold(),
                        Style::default().fg(Color::White).bold(),
                    ),
                    Status::Cancelled | Status::Skipped => (
                        Style::default().fg(theme.unknown),
                        Style::default().fg(Color::Rgb(75, 75, 85)),
                    ),
                    _ => (
                        Style::default().fg(Color::Rgb(60, 60, 75)),
                        Style::default().fg(Color::Rgb(75, 75, 90)),
                    ),
                };
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(animated_glyph(step.status, state.tick_count), glyph_style),
                    Span::styled(format!(" {}. ", si + 1), Style::default().fg(Color::Rgb(70, 70, 90))),
                    Span::styled(step.name.clone(), name_style),
                ])
            }).collect();

            f.render_widget(
                Paragraph::new(step_lines),
                areas[i * 2 + 1],
            );
        }
    }
}

fn render_diff(f: &mut Frame, area: Rect, state: &AppState) {
    let blk = styled_block("Diff vs last successful run", &state.theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    let Some(detail) = &state.run_detail else {
        let p = Paragraph::new("(no run loaded)");
        f.render_widget(p, inner);
        return;
    };
    let Some(wf) = state.workflow_for_runs.as_deref() else {
        let p = Paragraph::new("(no workflow context — diff needs to be opened from a workflow's run list)");
        f.render_widget(p, inner);
        return;
    };

    let baseline: Option<&HistoryEntry> = state
        .history
        .last_successful(wf)
        .filter(|e| e.run_id != detail.run.id);

    let header = Line::from(vec![
        Span::styled(
            format!("current run #{}", detail.run.id),
            Style::default().fg(Color::White).bold(),
        ),
        Span::raw("   vs   "),
        match &baseline {
            Some(b) => Span::styled(
                format!("last success #{}", b.run_id),
                Style::default().fg(Color::Green).bold(),
            ),
            None => Span::styled(
                "no successful run in history",
                Style::default().fg(Color::DarkGray).italic(),
            ),
        },
    ]);

    let mut lines: Vec<Line> = vec![header, Line::default()];

    // Build baseline lookup (job, step) -> Status.
    let mut baseline_steps: HashMap<(String, String), Status> = HashMap::new();
    if let Some(b) = baseline {
        for j in &b.jobs {
            for s in &j.steps {
                baseline_steps.insert((j.name.clone(), s.name.clone()), s.status);
            }
        }
    }

    let mut any_diff = false;
    for job in &detail.jobs {
        let mut job_lines: Vec<Line> = Vec::new();
        for step in &job.steps {
            let prev = baseline_steps
                .get(&(job.name.clone(), step.name.clone()))
                .copied();
            let changed = match prev {
                Some(p) => p != step.status,
                None => baseline.is_some(),
            };
            if !changed {
                continue;
            }
            any_diff = true;
            let prev_text = match prev {
                Some(p) => format!("{:?}", p),
                None => "(absent)".into(),
            };
            job_lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    animated_glyph(step.status, state.tick_count),
                    style_for_status(step.status, &state.theme),
                ),
                Span::raw(" "),
                Span::styled(step.name.clone(), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(prev_text, Style::default().fg(Color::DarkGray)),
                Span::raw(" → "),
                Span::styled(
                    format!("{:?}", step.status),
                    style_for_status(step.status, &state.theme),
                ),
            ]));
        }
        if !job_lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(animated_glyph(job.status, state.tick_count), style_for_status(job.status, &state.theme)),
                Span::raw(" "),
                Span::styled(job.name.clone(), Style::default().bold()),
            ]));
            lines.extend(job_lines);
            lines.push(Line::default());
        }
    }

    if !any_diff {
        if baseline.is_some() {
            lines.push(Line::from(Span::styled(
                "no step-status differences",
                Style::default().fg(Color::DarkGray).italic(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "browse a few completed runs first to populate history",
                Style::default().fg(Color::DarkGray).italic(),
            )));
        }
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn style_for_status(s: Status, theme: &Theme) -> Style {
    match s {
        Status::Success => Style::default().fg(theme.success).bold(),
        Status::Failure => Style::default().fg(theme.failure).bold(),
        Status::Running => Style::default().fg(theme.warning).bold(),
        Status::Queued => Style::default().fg(Color::Blue).bold(),
        Status::Cancelled => Style::default().fg(theme.unknown),
        Status::Skipped => Style::default().fg(theme.unknown),
        Status::Unknown => Style::default().fg(theme.unknown),
    }
}

fn row_bg_for_status(s: Status) -> Color {
    match s {
        Status::Failure            => Color::Rgb(45, 20, 20),
        Status::Running            => Color::Rgb(40, 36, 12),
        Status::Queued             => Color::Rgb(18, 20, 40),
        Status::Cancelled
        | Status::Skipped          => Color::Rgb(32, 32, 36),
        _                          => Color::Rgb(28, 30, 42),
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
        let p = Paragraph::new("(no prompt)").block(styled_block("Trigger", &state.theme));
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
    let has_options = prompt.fields.iter().any(|f| f.options.is_some());
    let submit_key = display_key(&state.keymap.tp_submit).to_string();
    let cancel_key = display_key(&state.keymap.back).to_string();
    let mut hint_spans = vec![
        Span::styled(submit_key, Style::default().fg(Color::White).bold()),
        Span::styled(" trigger  ", Style::default().fg(Color::DarkGray)),
        Span::styled(cancel_key, Style::default().fg(Color::White).bold()),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ];
    if has_options {
        hint_spans.push(Span::styled("  Space", Style::default().fg(Color::White).bold()));
        hint_spans.push(Span::styled(" cycle", Style::default().fg(Color::DarkGray)));
    }
    lines.push(Line::from(hint_spans));
    let p = Paragraph::new(lines)
        .block(styled_block(&title, &state.theme))
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

fn format_step_dur(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs.max(0.0))
    } else {
        let m = (secs / 60.0) as u64;
        let s = (secs % 60.0) as u64;
        if s == 0 { format!("{m}m") } else { format!("{m}m {s}s") }
    }
}

fn step_timing(
    step: &crate::provider::Step,
    max_secs: f64,
    tick: u64,
) -> Option<(String, String)> {
    use crate::provider::Status;
    let start = step.started_at?;
    let (secs, running) = if let Some(end) = step.completed_at {
        ((end - start).num_milliseconds().max(0) as f64 / 1000.0, false)
    } else {
        ((Utc::now() - start).num_milliseconds().max(0) as f64 / 1000.0,
         step.status == Status::Running)
    };
    let dur = format_step_dur(secs);
    let bar = if running {
        let n = ((tick / 3) % 11) as usize;
        format!("{}{}", "■".repeat(n), "□".repeat(10 - n))
    } else {
        let filled = if max_secs > 0.0 {
            ((secs / max_secs * 10.0).round() as usize).clamp(if secs > 0.0 { 1 } else { 0 }, 10)
        } else {
            0
        };
        format!("{}{}", "■".repeat(filled), "□".repeat(10 - filled))
    };
    Some((dur, bar))
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
            commit_msg: String::new(),
            status: Status::Skipped,
            created_at: created,
            updated_at: updated,
            url: String::new(),
            workflow_file: None,
        };
        // Skipped 5s after creation; should stay 5s no matter when we look.
        assert_eq!(elapsed_seconds(&run), 5);
    }
}
