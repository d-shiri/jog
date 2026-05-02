use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap,
};

use std::collections::HashMap;

use super::status_glyph;
use crate::app::state::{AppState, DetailItem, View, build_detail_items};
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
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let view_name = match state.view {
        View::Workflows => "Workflows",
        View::Runs => "Runs",
        View::RunDetail => "Run Detail",
        View::Logs => "Logs",
        View::Watch => "Watch",
        View::TriggerPrompt => "Trigger",
        View::Diff => "Diff",
    };
    let dot = Style::default().fg(Color::Rgb(55, 55, 80));
    let line = Line::from(vec![
        Span::styled(" ⚡ ", Style::default().fg(Color::Yellow)),
        Span::styled("jog", Style::default().fg(Color::White).bold()),
        Span::styled("  ·  ", dot),
        Span::styled(state.repo_label.as_str(), Style::default().fg(Color::White)),
        Span::styled("  ⎇ ", Style::default().fg(Color::Rgb(90, 110, 150))),
        Span::styled(state.current_branch.as_str(), Style::default().fg(Color::Yellow)),
        Span::styled("  ·  ", dot),
        Span::styled(view_name, Style::default().fg(Color::Cyan).bold()),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(18, 20, 32))),
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
            (display_key(&km.back).into(), "back"),
        ],
        View::RunDetail => vec![
            (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "step"),
            (format!("↵/{}", display_key(&km.open_logs)), "logs"),
            (display_key(&km.diff).into(), "diff"),
            (display_key(&km.back).into(), "back"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::Logs => {
            let np_label = if state.log_search_query.is_some() { "match" } else { "step" };
            vec![
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "scroll"),
                (format!("{}/{}", display_key(&km.page_down), display_key(&km.page_up)), "page"),
                (display_key(&km.scroll_top).into(), "top"),
                (format!("{}/{}", display_key(&km.next_step), display_key(&km.prev_step)), np_label),
                (display_key(&km.all_steps).into(), "all"),
                (display_key(&km.search).into(), "search"),
                (display_key(&km.back).into(), "back"),
                (display_key(&km.quit).into(), "quit"),
            ]
        },
        View::Watch => vec![
            (display_key(&km.back).into(), "back"),
            (display_key(&km.quit).into(), "quit"),
        ],
        View::Diff => vec![
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
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    render_workflows_list(f, chunks[0], state);
    render_workflows_preview(f, chunks[1], state);
}

fn render_workflows_list(f: &mut Frame, area: Rect, state: &AppState) {
    let count = state.workflows.len();
    let title = format!("Workflows ({count})");
    let blk = styled_block(&title);
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
                .unwrap_or_else(|| ("—".into(), Style::default().fg(Color::DarkGray)));
            let trig = if w.triggerable { "t" } else { " " };

            Row::new(vec![
                Cell::from(Span::styled(status_glyph(status), style_for_status(status))),
                Cell::from(Span::styled(w.name.clone(), Style::default())),
                Cell::from(Span::styled(
                    w.file_name.clone(),
                    Style::default().fg(Color::Rgb(110, 110, 140)),
                )),
                Cell::from(Span::styled(when_text, when_style)),
                Cell::from(Span::styled(
                    trig,
                    Style::default().fg(Color::Rgb(180, 140, 60)),
                )),
            ])
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

    let blk = styled_block(&title);
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
                Cell::from(Span::styled(status_glyph(r.status), style_for_status(r.status))),
                Cell::from(Span::styled(r.head_branch.clone(), Style::default().fg(Color::Yellow))),
                Cell::from(Span::styled(when_text, when_style)),
            ])
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
    f.render_stateful_widget(table, inner, &mut ts);
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
    let title = match &state.workflow_for_runs {
        Some(wf) => format!("Runs — {} ({})", wf, state.runs.len()),
        None => format!("Runs ({})", state.runs.len()),
    };
    let blk = styled_block(&title);
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
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = state
        .runs
        .iter()
        .map(|r| {
            let (when_text, when_style) = relative_styled(r.updated_at);
            Row::new(vec![
                Cell::from(Span::styled(status_glyph(r.status), style_for_status(r.status))),
                Cell::from(Span::styled(r.head_branch.clone(), Style::default().fg(Color::Yellow))),
                Cell::from(Span::styled(when_text, when_style)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(1),       // status glyph
        Constraint::Fill(1),         // branch
        Constraint::Length(10),      // updated
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

    let blk = styled_block(&title);
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
        for job in &detail.jobs {
            lines.push(Line::from(vec![
                Span::styled(status_glyph(job.status), style_for_status(job.status)),
                Span::raw(" "),
                Span::styled(job.name.clone(), Style::default().bold()),
            ]));
            for (si, step) in job.steps.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(status_glyph(step.status), style_for_status(step.status)),
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
            let p = Paragraph::new("loading…").block(styled_block("Run"));
            f.render_widget(p, area);
            return;
        }
    };

    let stats: HashMap<String, (u32, u32)> = state
        .workflow_for_runs
        .as_deref()
        .map(|wf| state.history.step_failure_stats(wf, 10))
        .unwrap_or_default();

    let items = build_detail_items(detail);
    let cursor = state.detail_cursor;
    let mut lines = Vec::new();

    for (flat_idx, item) in items.iter().enumerate() {
        let selected = flat_idx == cursor;
        match item {
            DetailItem::Job(ji) => {
                let job = &detail.jobs[*ji];
                let prefix = if selected { "▶ " } else { "  " };
                let name_style = if selected {
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                lines.push(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(status_glyph(job.status), style_for_status(job.status)),
                    Span::raw(" "),
                    Span::styled(job.name.clone(), name_style),
                ]));
            }
            DetailItem::Step { job: ji, step: si } => {
                let step = &detail.jobs[*ji].steps[*si];
                let prefix = if selected { "  ▶ " } else { "    " };
                let name_style = if selected {
                    Style::default().fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                let mut spans = vec![
                    Span::raw(prefix),
                    Span::styled(status_glyph(step.status), style_for_status(step.status)),
                    Span::raw(" "),
                    // Use sequential position (si+1) — step.number has gaps for
                    // internal composite-action sub-steps that GitHub hides from
                    // the job.steps list but still assigns numbers to.
                    Span::styled(format!("{}. {}", si + 1, step.name), name_style),
                ];
                if let Some((failed, total)) = stats.get(&step.name).copied() {
                    if failed > 0 {
                        let badge_style = if failed * 2 >= total {
                            Style::default().fg(Color::Red).bold()
                        } else {
                            Style::default().fg(Color::Yellow)
                        };
                        spans.push(Span::styled(
                            format!("  ({}/{} fails)", failed, total),
                            badge_style,
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }
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

/// Return `(Some("HH:MM:SS"), content)` when the line has the short time prefix
/// written by `clean_log_line`, otherwise `(None, whole_string)`.
fn split_time_prefix(s: &str) -> (Option<&str>, &str) {
    if s.len() > 9
        && s.as_bytes().get(2) == Some(&b':')
        && s.as_bytes().get(5) == Some(&b':')
        && s.as_bytes().get(8) == Some(&b' ')
        && s[..2].bytes().all(|b| b.is_ascii_digit())
        && s[3..5].bytes().all(|b| b.is_ascii_digit())
        && s[6..8].bytes().all(|b| b.is_ascii_digit())
    {
        (Some(&s[..8]), &s[9..])
    } else {
        (None, s)
    }
}

/// Parse ANSI SGR escape sequences in `line` and produce styled spans.
/// `default_style` is the baseline applied between resets (`\x1b[0m`).
fn ansi_line_to_spans(line: &str, default_style: Style) -> Vec<Span<'static>> {
    if !line.contains('\x1b') {
        return if line.is_empty() {
            vec![]
        } else {
            vec![Span::styled(line.to_string(), default_style)]
        };
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current = default_style;
    let chars: Vec<char> = line.chars().collect();
    let mut seg = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '[' {
            let text: String = chars[seg..i].iter().collect();
            if !text.is_empty() {
                spans.push(Span::styled(text, current));
            }
            let seq_start = i + 2;
            let mut j = seq_start;
            while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j < chars.len() && chars[j] == 'm' {
                let params: String = chars[seq_start..j].iter().collect();
                current = apply_sgr(&params, current, default_style);
            }
            i = j + 1;
            seg = i;
        } else {
            i += 1;
        }
    }
    let tail: String = chars[seg..].iter().collect();
    if !tail.is_empty() {
        spans.push(Span::styled(tail, current));
    }
    spans
}

fn apply_sgr(params: &str, current: Style, default: Style) -> Style {
    if params.is_empty() {
        return default;
    }
    let nums: Vec<u32> = params.split(';').filter_map(|s| s.parse().ok()).collect();
    let mut s = current;
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => s = default,
            1 => s = s.add_modifier(Modifier::BOLD),
            2 => s = s.add_modifier(Modifier::DIM),
            3 => s = s.add_modifier(Modifier::ITALIC),
            4 => s = s.add_modifier(Modifier::UNDERLINED),
            22 => s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => s = s.remove_modifier(Modifier::ITALIC),
            24 => s = s.remove_modifier(Modifier::UNDERLINED),
            30 => s = s.fg(Color::Black),
            31 => s = s.fg(Color::Red),
            32 => s = s.fg(Color::Green),
            33 => s = s.fg(Color::Yellow),
            34 => s = s.fg(Color::Blue),
            35 => s = s.fg(Color::Magenta),
            36 => s = s.fg(Color::Cyan),
            37 => s = s.fg(Color::Gray),
            38 if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() => {
                s = s.fg(Color::Rgb(nums[i+2] as u8, nums[i+3] as u8, nums[i+4] as u8));
                i += 4;
            }
            38 if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() => {
                s = s.fg(Color::Indexed(nums[i + 2] as u8));
                i += 2;
            }
            40 => s = s.bg(Color::Black),
            41 => s = s.bg(Color::Red),
            42 => s = s.bg(Color::Green),
            43 => s = s.bg(Color::Yellow),
            44 => s = s.bg(Color::Blue),
            45 => s = s.bg(Color::Magenta),
            46 => s = s.bg(Color::Cyan),
            47 => s = s.bg(Color::Gray),
            48 if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() => {
                s = s.bg(Color::Rgb(nums[i+2] as u8, nums[i+3] as u8, nums[i+4] as u8));
                i += 4;
            }
            48 if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() => {
                s = s.bg(Color::Indexed(nums[i + 2] as u8));
                i += 2;
            }
            90 => s = s.fg(Color::DarkGray),
            91 => s = s.fg(Color::LightRed),
            92 => s = s.fg(Color::LightGreen),
            93 => s = s.fg(Color::LightYellow),
            94 => s = s.fg(Color::LightBlue),
            95 => s = s.fg(Color::LightMagenta),
            96 => s = s.fg(Color::LightCyan),
            97 => s = s.fg(Color::White),
            _ => {}
        }
        i += 1;
    }
    s
}

/// Re-style spans of `line` so the (case-insensitive) `needle` is highlighted.
/// `current` picks the brighter highlight used for the active match.
fn highlight_line(line: Line<'static>, needle: &str, current: bool) -> Line<'static> {
    if needle.is_empty() {
        return line;
    }
    let hit_bg = if current {
        Color::Rgb(220, 200, 60)
    } else {
        Color::Rgb(120, 90, 30)
    };
    let hit_fg = Color::Black;

    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let text = span.content.into_owned();
        let style = span.style;
        let lower = text.to_lowercase();
        if !lower.contains(needle) {
            out.push(Span::styled(text, style));
            continue;
        }
        let bytes = text.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            match lower[cursor..].find(needle) {
                Some(rel) => {
                    let start = cursor + rel;
                    let end = start + needle.len();
                    if start > cursor {
                        out.push(Span::styled(text[cursor..start].to_string(), style));
                    }
                    out.push(Span::styled(
                        text[start..end].to_string(),
                        style.bg(hit_bg).fg(hit_fg).add_modifier(Modifier::BOLD),
                    ));
                    cursor = end;
                }
                None => {
                    out.push(Span::styled(text[cursor..].to_string(), style));
                    break;
                }
            }
        }
    }
    Line::from(out)
}

fn render_logs(f: &mut Frame, area: Rect, state: &AppState) {
    // Inner area = area minus the rounded border (1 row top + 1 row bottom).
    let viewport = area.height.saturating_sub(2);
    state.last_logs_viewport_height.set(viewport);

    let mut log_title = if let Some(idx) = state.log_section_idx {
        let name = state.log_sections.get(idx).map(|s| s.as_str()).unwrap_or("?");
        let total = state.log_sections.len();
        format!("Logs — {name}  [{}/{}]", idx + 1, total)
    } else {
        "Logs".to_string()
    };

    if let Some(buf) = &state.log_search_input {
        log_title.push_str(&format!("   /{buf}_"));
    } else if let Some(q) = &state.log_search_query {
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

    let sep = "─".repeat(area.width.saturating_sub(2) as usize);
    let time_style = Style::default().fg(Color::Rgb(80, 80, 80));

    let needle_lower = state
        .log_search_query
        .as_deref()
        .filter(|q| !q.is_empty())
        .map(|q| q.to_lowercase());
    let current_match_line = state
        .log_search_match_idx
        .and_then(|i| state.log_search_matches.get(i).copied());

    let body: Vec<Line> = state
        .log_lines
        .iter()
        .enumerate()
        .flat_map(|(src_idx, l)| {
            let (time, content) = split_time_prefix(l.as_str());
            let mk_time = || time.map(|t| Span::styled(format!("{t} "), time_style));

            let lines: Vec<Line> = if let Some(title) = content.strip_prefix("##[group]")
                .or_else(|| content.strip_prefix("##[section]"))
            {
                let title_style = Style::default().fg(Color::Cyan).bold();
                let mut header = vec![];
                if let Some(ts) = mk_time() { header.push(ts); }
                header.push(Span::styled("▸ ", title_style));
                header.extend(ansi_line_to_spans(title, title_style));
                vec![
                    Line::from(Span::styled(sep.clone(), Style::default().fg(Color::DarkGray))),
                    Line::from(header),
                    Line::default(),
                ]
            } else if content.starts_with("##[endgroup]") {
                vec![Line::default()]
            } else if let Some(cmd) = content.strip_prefix("##[command]") {
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("$ ", Style::default().fg(Color::Green).bold()));
                spans.extend(ansi_line_to_spans(cmd, Style::default().fg(Color::White)));
                vec![Line::from(spans)]
            } else if let Some(msg) = content.strip_prefix("##[error]") {
                let s = Style::default().fg(Color::Red).bold();
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("✗ ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                vec![Line::from(spans)]
            } else if let Some(msg) = content.strip_prefix("##[warning]") {
                let s = Style::default().fg(Color::Yellow);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("⚠ ", s.bold()));
                spans.extend(ansi_line_to_spans(msg, s));
                vec![Line::from(spans)]
            } else if let Some(msg) = content.strip_prefix("##[debug]") {
                let s = Style::default().fg(Color::DarkGray);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("# ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                vec![Line::from(spans)]
            } else if let Some(msg) = content.strip_prefix("##[notice]") {
                let s = Style::default().fg(Color::Cyan);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("ℹ ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                vec![Line::from(spans)]
            } else {
                // Keyword fallback only when ANSI codes are absent; otherwise let
                // the ANSI parser handle all coloring.
                let base = if !content.contains('\x1b') {
                    let lower = content.to_lowercase();
                    if lower.contains("error") || lower.contains("failed") {
                        Style::default().fg(Color::Red)
                    } else if lower.contains("warn") {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Rgb(200, 200, 200))
                    }
                } else {
                    Style::default().fg(Color::Rgb(200, 200, 200))
                };
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.extend(ansi_line_to_spans(content, base));
                vec![Line::from(spans)]
            };
            if let Some(needle) = needle_lower.as_deref() {
                let is_current = current_match_line == Some(src_idx);
                lines
                    .into_iter()
                    .map(|line| highlight_line(line, needle, is_current))
                    .collect::<Vec<Line>>()
            } else {
                lines
            }
        })
        .collect();

    let p = Paragraph::new(body)
        .block(styled_block(&log_title))
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

fn render_diff(f: &mut Frame, area: Rect, state: &AppState) {
    let blk = styled_block("Diff vs last successful run");
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
                    status_glyph(step.status),
                    style_for_status(step.status),
                ),
                Span::raw(" "),
                Span::styled(step.name.clone(), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(prev_text, Style::default().fg(Color::DarkGray)),
                Span::raw(" → "),
                Span::styled(
                    format!("{:?}", step.status),
                    style_for_status(step.status),
                ),
            ]));
        }
        if !job_lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(status_glyph(job.status), style_for_status(job.status)),
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
