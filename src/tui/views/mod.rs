use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, LineGauge, Padding, Paragraph, Row, Table, TableState,
    Wrap,
};

use std::collections::HashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::animated_glyph;
use super::motion::{Motion, mix};
use crate::app::state::{
    AppState, BatchPhase, DetailItem, GitOp, ItemState, Theme, View, build_detail_items,
};
use crate::history::HistoryEntry;
use crate::provider::github::{ApiFault, CRITICAL_PERCENT};
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
        View::Repos => render_repos(f, chunks[1], state),
        View::GitStatus => render_git_status(f, chunks[1], state),
        View::GitDiff => render_git_diff(f, chunks[1], state),
        View::Workflows => render_workflows(f, chunks[1], state),
        View::Runs => render_runs(f, chunks[1], state),
        View::RunDetail => render_run_detail(f, chunks[1], state),
        View::Logs => render_logs(f, chunks[1], state),
        View::Watch => render_watch(f, chunks[1], state),
        View::TriggerPrompt => render_trigger_prompt(f, chunks[1], state),
        View::Diff => render_diff(f, chunks[1], state),
        View::BatchCommit => render_batch_commit(f, chunks[1], state),
    }
    render_footer(f, chunks[2], state);
    if state.view == View::Logs {
        render_search_overlay(f, area, state);
    }
    if state.view == View::GitStatus {
        render_commit_overlay(f, area, state);
    }
    render_push_prompt(f, area, state);
    render_finder_overlay(f, area, state);
    // Drawn last so it sits above every other overlay.
    render_help_overlay(f, area, state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let sep = || Span::styled("  ›  ", Style::default().fg(theme.border_dim));
    let crumb: Vec<Span> = match state.view {
        // The panel below is titled "Repos" and counts them. Saying it twice
        // one line apart spends the width that the tallies now use.
        View::Repos => Vec::new(),
        View::GitStatus => vec![
            Span::styled("Repos", Style::default().fg(theme.text_muted)),
            sep(),
            Span::styled(
                state
                    .git_view
                    .as_ref()
                    .map(|g| g.spec.clone())
                    .unwrap_or_else(|| "?".into()),
                Style::default().fg(theme.text_muted),
            ),
            sep(),
            Span::styled("Changes", Style::default().fg(theme.primary).bold()),
        ],
        View::GitDiff => vec![
            Span::styled("Repos", Style::default().fg(theme.text_muted)),
            sep(),
            Span::styled(
                state
                    .git_diff
                    .as_ref()
                    .map(|d| d.spec.clone())
                    .unwrap_or_else(|| "?".into()),
                Style::default().fg(theme.text_muted),
            ),
            sep(),
            Span::styled("Changes", Style::default().fg(theme.text_muted)),
            sep(),
            Span::styled(
                state
                    .git_diff
                    .as_ref()
                    .map(|d| d.file.clone())
                    .unwrap_or_else(|| "?".into()),
                Style::default().fg(theme.primary).bold(),
            ),
        ],
        View::Workflows => vec![
            Span::styled("Workflows", Style::default().fg(theme.primary).bold()),
        ],
        View::Runs => vec![
            Span::styled("Workflows", Style::default().fg(theme.text_muted)),
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
                Span::styled("Workflows", Style::default().fg(theme.text_muted)),
                sep(),
                Span::styled(wf.to_string(), Style::default().fg(theme.text_muted)),
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
                Span::styled("Logs", Style::default().fg(theme.text_muted)),
                sep(),
                Span::styled(step.to_string(), Style::default().fg(theme.primary).bold()),
            ]
        }
        View::BatchCommit => {
            let n = state.batch.as_ref().map(|b| b.items.len()).unwrap_or(0);
            vec![
                Span::styled("Repos", Style::default().fg(theme.text_muted)),
                sep(),
                Span::styled(
                    format!("Batch commit ({n})"),
                    Style::default().fg(theme.primary).bold(),
                ),
            ]
        }
        View::Watch         => vec![Span::styled("Watch",   Style::default().fg(theme.primary).bold())],
        View::Diff          => vec![Span::styled("Diff",    Style::default().fg(theme.primary).bold())],
        View::TriggerPrompt => vec![Span::styled("Trigger", Style::default().fg(theme.primary).bold())],
    };

    let dot = Style::default().fg(theme.border_dim);
    let mut spans = vec![
        Span::styled(" ⚡ ", Style::default().fg(theme.accent)),
        Span::styled("jog", Style::default().fg(theme.text_bright).bold()),
        Span::styled(
            format!(" {}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.text_faint),
        ),
        Span::styled("  ·  ", dot),
    ];
    // On the workspace dashboard there is no single active repo yet — naming one
    // (and its branch) would be arbitrary. Show where we're scanning instead.
    match (&state.workspace_root, state.view) {
        (Some(root), View::Repos | View::GitStatus | View::GitDiff) => spans.push(Span::styled(
            short_path(root),
            Style::default().fg(theme.text_bright),
        )),
        _ => {
            spans.push(Span::styled(
                state.repo_label.as_str(),
                Style::default().fg(theme.text_bright),
            ));
            spans.push(Span::styled("  ⎇ ", Style::default().fg(theme.info)));
            spans.push(Span::styled(
                state.current_branch.as_str(),
                Style::default().fg(theme.accent),
            ));
        }
    }
    if !crumb.is_empty() {
        spans.push(Span::styled("  ·  ", dot));
        spans.extend(crumb);
    }
    // The dashboard no longer wears a frame saying "Repos 8", so the total comes
    // up here — next to the directory it counts, which is what it is about. The
    // tallies do not cover it: a checkout that has never run CI lands in none of
    // the four, and "7 of 8" is a different sentence from "7".
    if state.view == View::Repos && !state.repos.is_empty() {
        spans.push(Span::styled("  ·  ", dot));
        spans.push(Span::styled(
            format!("{} repos", state.repos.len()),
            Style::default().fg(theme.text_muted),
        ));
    }

    // The right corner is for the state of the machinery rather than of the
    // work: how the workspace stands, how much API budget is left, when the
    // next refresh lands, and what time it is — a run that "finished 3m ago"
    // means nothing without the last of those.
    let width = |v: &[Span]| v.iter().map(|s| s.width()).sum::<usize>();
    let left_w = width(&spans);
    let tally = workspace_tallies(state);
    let dirty = uncommitted_span(state);
    let tail = {
        let mut t = quota_spans(state);
        t.extend([
            Span::styled(poll_clock(state), Style::default().fg(theme.text_faint)),
            Span::styled("   ", Style::default()),
            Span::styled(
                chrono::Local::now().format("%H:%M").to_string(),
                Style::default().fg(theme.text_muted),
            ),
            Span::raw(" "),
        ]);
        t
    };
    // Shed from the least load-bearing end inwards rather than letting the two
    // ends meet in the middle: an overlap reads as a bug, a missing figure reads
    // as a small window.
    let total = area.width as usize;
    let mut right: Vec<Span> = Vec::new();
    for part in [&tally, &dirty] {
        if left_w + width(&right) + width(part) + width(&tail) + 2 <= total {
            right.extend(part.iter().cloned());
        }
    }
    right.extend(tail);
    let right_w = width(&right);

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.surface)),
        area,
    );

    // The widest empty space on the screen sat between the two, and what belongs
    // in it is whatever is happening right now — which otherwise only exists in
    // a strip along the bottom of one view. Centred in the gap the two ends
    // leave rather than on the screen, so a long path shifts it along instead of
    // pushing it off; dropped whole the moment the gap will not hold it.
    const GUTTER: usize = 3;
    let mid = now_playing(state);
    let mid_w = width(&mid);
    let gap_start = left_w + GUTTER;
    let gap_end = total.saturating_sub(right_w + GUTTER);
    if mid_w > 0 && gap_end > gap_start && gap_end - gap_start >= mid_w {
        let x = gap_start + (gap_end - gap_start - mid_w) / 2;
        let slot = Rect {
            x: area.x + x as u16,
            width: mid_w as u16,
            ..area
        };
        f.render_widget(Paragraph::new(Line::from(mid)), slot);
    }
    f.render_widget(Paragraph::new(Line::from(right)).right_aligned(), area);
}

/// What is happening right now, for the middle of the header.
///
/// One line, and only ever one: a header that grows a list is a header you have
/// to read. Trouble outranks progress — a run in flight is worth knowing about,
/// but not while eight rows cannot be fetched at all.
fn now_playing(state: &AppState) -> Vec<Span<'static>> {
    let theme = &state.theme;
    if Tallies::of(state).broken > 0
        && let Some(detail) = shared_fault_detail(state)
    {
        return vec![
            Span::styled("⚠ ", Style::default().fg(theme.failure).bold()),
            Span::styled(detail, Style::default().fg(theme.failure)),
        ];
    }
    // Your own push outranks the ambient traffic: the ⇡ chip follows it from
    // "waiting for CI to notice" through the run it spawned, and vanishes the
    // moment that run settles (which is also when the notification fires).
    if let Some(w) = state.push_watches.first() {
        let name = w.spec.rsplit('/').next().unwrap_or(&w.spec).to_string();
        let mut out = vec![
            Span::styled("⇡ ", Style::default().fg(theme.accent).bold()),
            Span::styled(name, Style::default().fg(theme.text_bright).bold()),
        ];
        match &w.run {
            None => {
                out.push(Span::styled(
                    "  pushed — waiting for CI ",
                    Style::default().fg(theme.text_muted),
                ));
                out.push(Span::styled(
                    animated_glyph(Status::Running, state.tick_count),
                    Style::default().fg(theme.text_faint),
                ));
            }
            Some(run) => {
                out.push(Span::styled(
                    format!("  {} ", animated_glyph(run.status, state.tick_count)),
                    Style::default().fg(theme.warning).bold(),
                ));
                out.push(Span::styled(
                    truncate(&run.display_title, 28),
                    Style::default().fg(theme.text_muted),
                ));
                out.push(Span::styled(
                    format!("  {}", compact_elapsed(run.created_at)),
                    Style::default().fg(theme.text_faint),
                ));
            }
        }
        if state.push_watches.len() > 1 {
            out.push(Span::styled(
                format!("  +{}", state.push_watches.len() - 1),
                Style::default().fg(theme.text_faint),
            ));
        }
        return out;
    }
    let live = state.active_progress();
    let Some((card, detail)) = live.first() else {
        return Vec::new();
    };
    // The owner is the same for nearly every row and is already in the path; the
    // name is the part that says which one.
    let name = card.spec.rsplit('/').next().unwrap_or(&card.spec).to_string();
    let mut out = vec![
        Span::styled(
            format!("{} ", animated_glyph(Status::Running, state.tick_count)),
            Style::default().fg(theme.warning).bold(),
        ),
        Span::styled(name, Style::default().fg(theme.text_bright).bold()),
        Span::styled("  ", Style::default()),
        Span::styled(
            truncate(&detail.run.display_title, 28),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(
            format!("  {}", compact_elapsed(detail.run.created_at)),
            Style::default().fg(theme.text_faint),
        ),
    ];
    if live.len() > 1 {
        out.push(Span::styled(
            format!("  +{}", live.len() - 1),
            Style::default().fg(theme.text_faint),
        ));
    }
    out
}

/// How long something has been going, in as few characters as say it.
///
/// No "ago": this is a stopwatch on something still running, not a timestamp.
fn compact_elapsed(t: chrono::DateTime<Utc>) -> String {
    let secs = (Utc::now() - t).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// The workspace at a glance: how CI stands, and how much is uncommitted.
///
/// The same figures the dashboard's title bar used to carry, moved up a line so
/// they survive walking into a run's logs — which is exactly when a second repo
/// going red is easiest to miss.
fn workspace_tallies(state: &AppState) -> Vec<Span<'static>> {
    let theme = &state.theme;
    let t = Tallies::of(state);
    if state.repos.is_empty() {
        return Vec::new();
    }
    // A zero is the absence of a thing, not an instance of it. Painting `✗0` in
    // failure red puts a red mark in the corner of a workspace where nothing is
    // wrong, and a colour that means "this counter exists" teaches you to stop
    // reading the colour.
    let count = |n: u32, text: String, on: Color, strong: bool| {
        let mut style = Style::default().fg(if n == 0 { theme.text_faint } else { on });
        if strong && n > 0 {
            style = style.bold();
        }
        Span::styled(text, style)
    };
    let mut out = vec![
        count(t.ok, format!("✓{}", t.ok), theme.success, false),
        count(t.fail, format!("  ✗{}", t.fail), theme.failure, false),
    ];
    if t.busy > 0 {
        out.push(count(t.busy, format!("  ⏵{}", t.busy), theme.warning, true));
    }
    if t.broken > 0 {
        out.push(count(t.broken, format!("  !{}", t.broken), theme.failure, true));
    }
    out.push(Span::styled("   ", Style::default()));
    out
}

/// How much work is sitting uncommitted across the workspace.
///
/// Files, not repos: "3 repos have changes" is the number you already know from
/// the column, and it does not tell you which way the afternoon went. Kept apart
/// from the tallies because it is the widest thing in the corner and so the
/// first that a narrow terminal should lose.
fn uncommitted_span(state: &AppState) -> Vec<Span<'static>> {
    let dirty: usize = state
        .repos
        .iter()
        .filter_map(|c| c.git.as_ref())
        .map(|g| g.staged_count() + g.unstaged_count())
        .sum();
    if dirty == 0 {
        return Vec::new();
    }
    vec![Span::styled(
        format!("◆{dirty} uncommitted   "),
        Style::default().fg(state.theme.accent),
    )]
}

/// A path short enough for a header: home-relative, and no deeper than the two
/// directories that actually distinguish it.
///
/// The full path is the same forty characters on every screen and answers a
/// question nobody asks twice; the tail is what says which checkout you are in.
fn short_path(p: &std::path::Path) -> String {
    let full = p.display().to_string();
    let shown = match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(home) if !home.is_empty() && full.starts_with(&home) => {
            format!("~{}", &full[home.len()..])
        }
        _ => full,
    };
    let parts: Vec<&str> = shown.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        return shown;
    }
    format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

/// The API budget, as a share spent.
///
/// In the corner because it is not what you came to look at — right up until it
/// is the only thing that matters, and by then a number you have to go and ask
/// for is a number nobody asked for. The colour does the work: quiet while
/// there is room, warming as it fills, red at the point where the useful move
/// is to quit and let the hour run out.
fn quota_spans(state: &AppState) -> Vec<Span<'static>> {
    let theme = &state.theme;
    let Some(q) = state.quota else {
        return Vec::new();
    };
    let pct = q.percent();
    let color = quota_color(theme, pct);
    let mut style = Style::default().fg(color);
    if q.is_critical() {
        style = style.bold();
    }
    let mut spans = vec![Span::styled(format!("API {pct}%"), style)];
    // The reset clock only once it is the answer to a question you now have:
    // below the line it is noise, above it it is how long to stay away.
    if q.is_critical() {
        spans.push(Span::styled(
            format!(" · till {}", q.reset.with_timezone(&Local).format("%H:%M")),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled("   ", Style::default()));
    spans
}

/// Quiet, then yellow, then red — reaching full red exactly where the alarm is.
///
/// A ramp rather than three steps: the number that matters is not 90, it is
/// whether the last few polls moved it a lot, and a colour that is already
/// halfway to red says that without arithmetic.
fn quota_color(theme: &Theme, pct: u32) -> Color {
    const CALM: u32 = 50;
    if pct < CALM {
        return theme.text_faint;
    }
    let t = (pct - CALM) as f64 / (CRITICAL_PERCENT - CALM) as f64;
    mix(theme.warning, theme.failure, t)
}

/// A quarter-turn clock face counting down to the next poll.
///
/// A glyph rather than a number of seconds: the exact figure is never the
/// question, only whether what is on screen was fetched recently — and a
/// countdown you can read at a glance answers that without being read.
///
/// One fixed template — a glyph column and a right-aligned seconds field — so
/// every character position keeps one role and nothing in the corner ever
/// shifts. A fetch swaps the draining clock face for a spinner and changes
/// nothing else: the countdown to the next poll stays up, because it stays
/// true. Only a fetch that drags past a few seconds takes over the number
/// field — progress ("3/8") or elapsed time — which is exactly when the
/// anomaly *should* displace the routine.
fn poll_clock(state: &AppState) -> String {
    const FACES: [&str; 4] = ["◴", "◵", "◶", "◷"];
    // Not the countdown faces: spinning and draining on the same glyphs would
    // leave nothing to say which state the corner is in.
    const SPINNER: [&str; 4] = ["⠋", "⠙", "⠸", "⠴"];
    // Git mutations parked in hooks sit in `pending` too, but they fetch
    // nothing — counting them here would blame the provider for a slow
    // pre-commit hook.
    let in_hooks = state.git_ops.values().filter(|o| !o.finished).count();
    let inflight = state.pending.saturating_sub(in_hooks);
    let elapsed = state.tick_count.saturating_sub(state.last_poll_tick);
    let left = state.poll_ticks.saturating_sub(elapsed);
    if inflight > 0 {
        if state.fetch_hwm.get() == 0 {
            state.fetch_started_tick.set(state.tick_count);
        }
        let total = state.fetch_hwm.get().max(inflight);
        state.fetch_hwm.set(total);
        let spin = SPINNER[(state.tick_count / 3 % 4) as usize];
        let secs = state.tick_count.saturating_sub(state.fetch_started_tick.get()) / 10;
        if secs >= 3 {
            let field = if total > 1 {
                format!("{}/{total}", total - inflight)
            } else {
                format!("{secs}s")
            };
            return format!("{spin} {field:>4}");
        }
        return format!("{spin} {:>3}s", left.div_ceil(10));
    }
    state.fetch_hwm.set(0);
    let quarter = ((left * 4) / state.poll_ticks.max(1)).min(3) as usize;
    format!("{} {:>3}s", FACES[3 - quarter], left.div_ceil(10))
}

pub(super) fn display_key(s: &str) -> &str {
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

    // The push question owns the keyboard the same way, and its keys are not
    // the working tree's.
    if state.push_prompt.is_some() {
        let spans = vec![
            Span::raw(" "),
            Span::styled("↵", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("take the highlighted answer", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("y/n", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("answer outright", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("←/→", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("switch", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("Esc", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("not now", Style::default().fg(theme.text_muted)),
        ];
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.surface)),
            area,
        );
        return;
    }

    // The finder overlay owns the keyboard while it's up, so advertise its keys
    // instead of the view's.
    if state.finder.is_some() {
        let spans = vec![
            Span::raw(" "),
            Span::styled("type", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("filter", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("↑/↓", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("move", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("↵", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("select", Style::default().fg(theme.text_muted)),
            Span::raw("  "),
            Span::styled("Esc", Style::default().fg(theme.text_bright).bold()),
            Span::raw(" "),
            Span::styled("cancel", Style::default().fg(theme.text_muted)),
        ];
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.surface)),
            area,
        );
        return;
    }

    let hints: Vec<(String, &str)> = match state.view {
        View::Repos => {
            let mut hints = vec![
                ("↵".into(), "open repo"),
                (display_key(&km.git_view).into(), "changes"),
                (
                    display_key(&km.repo_mark).into(),
                    if state.repo_marks.is_empty() { "mark" } else { "mark/unmark" },
                ),
            ];
            // Only advertised once marking it would do something — the key is
            // inert with an empty selection, and a dead key in the footer reads
            // as a broken one.
            if !state.repo_marks.is_empty() {
                hints.push((display_key(&km.batch_commit).into(), "commit marked"));
            }
            hints.push((display_key(&km.finder).into(), "find"));
            hints.push((display_key(&km.open_browser).into(), "open"));
            hints.push((display_key(&km.quit).into(), "quit"));
            hints
        }
        View::BatchCommit => match state.batch.as_ref().map(|b| b.phase) {
            Some(BatchPhase::Compose) => vec![
                ("type".into(), "message for every repo"),
                ("Bksp".into(), "delete"),
                ("↵".into(), "start"),
                ("Esc".into(), "cancel"),
            ],
            Some(BatchPhase::Paused) => vec![
                (display_key(&km.batch_retry).into(), "retry"),
                (display_key(&km.batch_skip).into(), "skip"),
                (display_key(&km.git_view).into(), "open repo"),
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "scroll"),
                (
                    format!("{}/{}", display_key(&km.next_error), display_key(&km.prev_error)),
                    "next/prev error",
                ),
                (display_key(&km.back).into(), "stop"),
            ],
            Some(BatchPhase::AskPush) => vec![
                (display_key(&km.git_push).into(), "push all"),
                (display_key(&km.back).into(), "done, don't push"),
            ],
            Some(BatchPhase::Done) => vec![(display_key(&km.back).into(), "back to repos")],
            _ => vec![
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "scroll"),
                (display_key(&km.yank).into(), "yank output"),
                (display_key(&km.back).into(), "stop after this repo"),
            ],
        },
        View::GitStatus if state.git_view.as_ref().is_some_and(|g| g.commit_input.is_some()) => {
            vec![
                ("type".into(), "message"),
                ("Bksp".into(), "delete"),
                ("↵".into(), "commit"),
                ("Esc".into(), "cancel"),
            ]
        }
        // Hook output on screen: the movement keys drive it, so say so rather
        // than list the file-list keys it has taken over.
        View::GitStatus if state.current_op().is_some() => {
            let running = state.current_op().is_some_and(|o| !o.finished);
            let mut hints = vec![
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "scroll"),
                (
                    format!("{}/{}", display_key(&km.next_error), display_key(&km.prev_error)),
                    "next/prev error",
                ),
                (
                    format!("{}/{}", display_key(&km.scroll_top), display_key(&km.scroll_bottom)),
                    "top/tail",
                ),
                (display_key(&km.yank).into(), "yank output"),
            ];
            hints.push((
                display_key(&km.back).into(),
                if running { "leave (keeps running)" } else { "dismiss" },
            ));
            hints
        }
        View::GitStatus => {
            let mut hints = vec![
                (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "move"),
                (display_key(&km.git_stage).into(), "stage/unstage"),
                (display_key(&km.git_stage_all).into(), "stage all"),
                (display_key(&km.git_commit).into(), "commit"),
                (display_key(&km.git_push).into(), "push"),
            ];
            if state.git_view.as_ref().is_some_and(|g| g.has_ci) {
                hints.push((display_key(&km.trigger).into(), "run CI"));
                // The same key, but the honest label: it opens the PR that
                // exists, or the page that creates the one that doesn't.
                let label = if state.git_view.as_ref().is_some_and(|g| g.open_pr().is_some()) {
                    "open PR"
                } else {
                    "new PR"
                };
                hints.push((display_key(&km.open_browser).into(), label));
            }
            hints.push((display_key(&km.git_refresh).into(), "refresh"));
            hints.push((display_key(&km.back).into(), "back"));
            hints
        }
        View::GitDiff => vec![
            (format!("{}/{}", display_key(&km.down), display_key(&km.up)), "scroll"),
            (format!("{}/{}", display_key(&km.page_down), display_key(&km.page_up)), "page"),
            (
                format!("{}/{}", display_key(&km.next_step), display_key(&km.prev_step)),
                "next/prev file",
            ),
            (display_key(&km.git_stage).into(), "stage/unstage"),
            (display_key(&km.back).into(), "back"),
        ],
        View::Workflows => vec![
            ("↵".into(), "runs"),
            (display_key(&km.trigger).into(), "trigger"),
            (display_key(&km.watch).into(), "watch"),
            (display_key(&km.finder).into(), "find"),
            (display_key(&km.repos_view).into(), "repos"),
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
            (display_key(&km.finder).into(), "find"),
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
                (
                    format!("{}/{}", display_key(&km.next_error), display_key(&km.prev_error)),
                    "error",
                ),
                (
                    display_key(&km.log_focus).into(),
                    if state.log_focus { "focus ✓" } else { "focus" },
                ),
            ];
            if state.log_focus && !state.log_fold_rows.is_empty() {
                hints.push(("↵".into(), "show hidden"));
            } else if !state.log_groups.is_empty() {
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

    // `?` is the discovery entry point, so it belongs on every view's footer.
    let mut hints = hints;
    hints.push((display_key(&km.help).into(), "help"));

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(key.clone(), Style::default().fg(theme.text_bright).bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(theme.text_muted)));
    }

    // What a batch commit would take, sitting beside the key that would take it
    // — the marks are otherwise a column you have to scan for.
    if state.view == View::Repos && !state.repo_marks.is_empty() {
        spans.push(Span::styled(
            format!("   ◆ {} marked", state.repo_marks.len()),
            Style::default().fg(theme.accent).bold(),
        ));
    }

    if state.pending > 0 {
        spans.push(Span::styled(
            format!("  {}", Motion::new(state.tick_count).spinner()),
            Style::default().fg(theme.accent),
        ));
    }

    if let Some(msg) = &state.status_msg {
        spans.push(Span::styled("   │   ", Style::default().fg(theme.border)));
        spans.push(Span::styled(msg.clone(), Style::default().fg(theme.text_bright)));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.surface)),
        area,
    );
}

/// One panel, drawn the same way everywhere.
///
/// The name sits in a chip cut into the top rule, and anything summarising the
/// panel goes to the *right* end of that rule. Every panel used to pack its
/// title, its counts and its badges into the left corner and leave three
/// quarters of the border empty, which is why a wide terminal made jog look
/// like it was designed for an 80-column one.
fn panel<'a>(name: &str, right: Vec<Span<'a>>, theme: &Theme, accent: Color) -> Block<'a> {
    let chip = Line::from(vec![
        Span::styled("─┤ ", Style::default().fg(theme.border)),
        Span::styled(name.to_string(), Style::default().fg(accent).bold()),
        Span::styled(" ├", Style::default().fg(theme.border)),
    ]);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(chip);
    if !right.is_empty() {
        let mut spans = vec![Span::styled("─ ", Style::default().fg(theme.border))];
        spans.extend(right);
        spans.push(Span::styled(" ─", Style::default().fg(theme.border)));
        block = block.title(Line::from(spans).right_aligned());
    }
    block
}

fn styled_block<'a>(title: &str, theme: &Theme) -> Block<'a> {
    panel(title, Vec::new(), theme, theme.primary)
}

/// What a view says when it has nothing to show.
///
/// Centred, and always three things: a glyph, one sentence of what is not here,
/// and the way to change that. A grey italic line in the top-left corner reads
/// as the app failing to draw something; this reads as an answer to a question.
fn render_empty(f: &mut Frame, area: Rect, theme: &Theme, glyph: &str, what: &str, how: &str) {
    if area.height < 3 || area.width < 20 {
        // No room to be graceful; the sentence still beats nothing.
        f.render_widget(
            Paragraph::new(Span::styled(what.to_string(), Style::default().fg(theme.text_faint)))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let lines = vec![
        Line::from(Span::styled(
            glyph.to_string(),
            Style::default().fg(theme.text_ghost),
        ))
        .centered(),
        Line::raw(""),
        Line::from(Span::styled(
            what.to_string(),
            Style::default().fg(theme.text_muted),
        ))
        .centered(),
        Line::from(Span::styled(
            how.to_string(),
            Style::default().fg(theme.text_faint).italic(),
        ))
        .centered(),
    ];
    let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
    let box_area = Rect {
        y: top,
        height: (lines.len() as u16).min(area.height),
        ..area
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), box_area);
}

/// The forge a row lives on, as one dim glyph before its name.
///
/// Dim on purpose: it is a label, not a status, and the row already has two
/// columns whose job is to be noticed. A checkout with no remote we recognise
/// gets blank space of the same width, so the names stay in one column —
/// ragged left edges are harder to read down than a repeated mark.
///
/// Only GitHub today. `remote` is set from an origin the GitHub provider could
/// parse, so its presence is the whole test; a second forge would add its glyph
/// here and nothing else would move.
fn forge_span(card: &crate::app::state::RepoCard, state: &AppState, theme: &Theme) -> Span<'static> {
    if state.forge_icon.is_empty() {
        return Span::raw("");
    }
    match card.remote {
        Some(_) => Span::styled(
            format!("{} ", state.forge_icon),
            Style::default().fg(theme.text_faint),
        ),
        None => Span::raw(" ".repeat(state.forge_icon.chars().count() + 1)),
    }
}

/// How the watched repos stand: the four numbers the header carries.
#[derive(Debug, Clone, Copy, Default)]
struct Tallies {
    ok: u32,
    fail: u32,
    busy: u32,
    /// Rows that could not be fetched at all — a state of jog, not of CI.
    broken: u32,
}

impl Tallies {
    fn of(state: &AppState) -> Self {
        state.repos.iter().fold(Self::default(), |mut t, c| {
            if c.error.is_some() {
                t.broken += 1;
                return t;
            }
            match c.latest_status() {
                Some(Status::Success) => t.ok += 1,
                Some(Status::Failure) => t.fail += 1,
                Some(Status::Running) | Some(Status::Queued) => t.busy += 1,
                _ => {}
            }
            t
        })
    }
}

/// The one reason every broken row is broken, worded for a header.
///
/// `None` when the failures disagree — then no single line is true of all of
/// them and the rows have to speak for themselves.
fn shared_fault_detail(state: &AppState) -> Option<String> {
    let mut errs = state.repos.iter().filter_map(|c| c.error.as_ref());
    let first = errs.next()?;
    if !errs.all(|e| e.fault == first.fault && e.text == first.text) {
        return None;
    }
    let detail = match first.fault {
        // A wait is only bad news until you know how long it is.
        ApiFault::RateLimited => match state.quota.map(|q| q.reset).filter(|t| *t > Utc::now()) {
            Some(t) => format!(
                "rate limited · retry {}",
                t.with_timezone(&Local).format("%H:%M")
            ),
            None => format!("rate limited · {}", ApiFault::RateLimited.detail()),
        },
        // Not the hourly budget — the meter two inches to the right can read 4%
        // while this is happening, and pointing at the hour's reset would offer
        // a fifty-minute wait for something that clears in one. What it needs to
        // say is that jog is the one holding off, and for how long.
        ApiFault::Throttled => match state.api_hold_left() {
            Some(secs) => format!("asked too fast · retrying in {}", format_countdown(secs)),
            None => format!("throttled · {}", ApiFault::Throttled.detail()),
        },
        // Nothing canned to add — the message already is the specific one.
        ApiFault::Other => first.text.clone(),
        fault => format!("{} · {}", fault.label(), fault.detail()),
    };
    Some(truncate(&detail, 60))
}

/// A wait short enough to sit through, worded as one.
fn format_countdown(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn render_repos(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;

    // Anything mid-flight gets its own strip along the bottom. It is given room
    // only when the table can still show a useful number of rows — the list is
    // the view, and a live panel that squeezes it out defeats the point.
    let live = state.active_progress();
    let strip_rows = live.len().min(4) as u16;
    let (area, strip_area) = if strip_rows == 0 || area.height < strip_rows + 7 {
        (area, None)
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(strip_rows + 2)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    };

    if let Some(sa) = strip_area {
        render_activity_strip(f, sa, state, &live);
    }

    // No frame. The header one line up already names the directory and counts
    // what is in it, so a rounded box titled "Repos 8" spent two rows and two
    // columns restating it — and the rows are the view. What the border also
    // carried now lives where it keeps working after you leave: the shared
    // fault in the header, the marked count in the footer beside the key that
    // acts on it. Only the side margin survives, so names never touch the edge.
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };

    if inner.height < 2 || inner.width < 8 {
        return;
    }

    if state.repos.is_empty() {
        render_empty(
            f,
            inner,
            theme,
            "▢",
            "No repos to watch yet.",
            "add repos = [\"owner/name\", …] under [provider] in config.toml, or run jog from a directory of checkouts",
        );
        return;
    }

    // What fits. Columns are dropped by how much they add per column of width,
    // rather than letting every one of them shrink until they all truncate —
    // eight half-legible columns are worse than five whole ones.
    let cols = Columns::for_width(inner.width);
    let spark_w = cols.spark_w;

    // Faint rather than muted: a column head is read once, when you are learning
    // the table, and never again. At the same weight as the data it labels it
    // competes with it on every glance after that.
    let hdr = Style::default().fg(theme.text_faint);
    let mut header_cells = vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from(Span::styled("Repo", hdr)),
        Cell::from(Span::styled("Local branch", hdr)),
        Cell::from(Span::styled("Changes", hdr)),
        Cell::from(Span::styled("Latest run", hdr)),
    ];
    if cols.ran_on {
        header_cells.push(Cell::from(Span::styled("Ran on", hdr)));
    }
    if cols.updated {
        // Over a right-aligned column, so the label sits over its own digits.
        header_cells.push(Cell::from(
            Line::from(Span::styled("Updated", hdr)).right_aligned(),
        ));
    }
    if cols.recent {
        header_cells.push(Cell::from(Span::styled("Recent runs", hdr)));
    }
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    // Marked for a batch commit. Its own column rather than a prefix on the
    // name, so the marks line up and can be counted down the list at a glance.
    let mark_cell = |card: &crate::app::state::RepoCard| -> Cell<'static> {
        if state.repo_marks.contains(&card.spec) {
            Cell::from(Span::styled("◆", Style::default().fg(theme.accent).bold()))
        } else {
            Cell::from("")
        }
    };

    // Which branch you are standing on, and how far it has drifted from
    // upstream. Distinct from the "Ran on" column, which reports the branch the
    // latest CI run used. How *dirty* the tree is lives in its own column, so a
    // long branch name can never push the file count out of sight.
    let local_cell = |card: &crate::app::state::RepoCard| -> Cell<'static> {
        let Some(g) = card.git.as_ref() else {
            return Cell::from("");
        };
        let mut spans: Vec<Span> = vec![Span::styled(
            truncate(&g.branch, 20),
            Style::default().fg(theme.accent),
        )];
        if g.ahead > 0 {
            spans.push(Span::styled(
                format!(" ↑{}", g.ahead),
                Style::default().fg(theme.accent),
            ));
        }
        if g.behind > 0 {
            spans.push(Span::styled(
                format!(" ↓{}", g.behind),
                Style::default().fg(theme.text_muted),
            ));
        }
        // A commit running (or stopped by a hook) in another repo is reported
        // on its own row — otherwise the only way to learn that a 90-second
        // `pre-commit` is still going is to walk back into that repo and look.
        if let Some(op) = state.git_ops.get(&card.spec) {
            spans.push(op_row_marker(op, state.tick_count, theme));
        }
        Cell::from(Line::from(spans))
    };

    // How dirty the working tree is, split the same way the Changes view splits
    // it: what is already staged, and what still isn't. A single lump count
    // can't tell "31 files ready to commit" from "31 files untouched since the
    // last commit", and those call for opposite next moves.
    let changes_cell = |card: &crate::app::state::RepoCard| -> Cell<'static> {
        let Some(g) = card.git.as_ref() else {
            return Cell::from("");
        };
        if g.is_clean() {
            return Cell::from(Line::from(vec![
                Span::styled("✓ ", Style::default().fg(theme.success)),
                Span::styled("clean", Style::default().fg(theme.success_dim)),
            ]));
        }
        let staged = g.staged_count();
        let unstaged = g.unstaged_count();
        let mut spans: Vec<Span> = Vec::new();
        if staged > 0 {
            spans.push(Span::styled(
                format!("✚{staged}"),
                Style::default().fg(theme.success).bold(),
            ));
        }
        if unstaged > 0 {
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("●{unstaged}"),
                Style::default().fg(theme.warning).bold(),
            ));
        }
        Cell::from(Line::from(spans))
    };

    // The run's branch, flagged when it isn't the branch you're standing on —
    // that mismatch is exactly the "why does this say dependabot?" confusion.
    let ran_on_cell = |card: &crate::app::state::RepoCard, run_branch: &str| -> Cell<'static> {
        // No runs yet means no branch to report — and nothing to diverge from.
        if run_branch.is_empty() {
            return Cell::from(Span::styled("—", Style::default().fg(theme.unknown)));
        }
        let local = card.git.as_ref().map(|g| g.branch.as_str());
        let diverged = local.is_some_and(|b| b != run_branch);
        let style = if diverged {
            Style::default().fg(theme.accent_dim)
        } else {
            Style::default().fg(theme.accent)
        };
        // The marker leads rather than trails: a long branch name would clip a
        // trailing one off the end of the column, and leading keeps the markers
        // aligned so divergent rows are scannable down the list.
        let marker = if diverged {
            Span::styled("≠ ", Style::default().fg(theme.accent_dim).bold())
        } else {
            Span::raw("  ")
        };
        Cell::from(Line::from(vec![
            marker,
            Span::styled(truncate(run_branch, 18), style),
        ]))
    };

    // Rows for repos, optionally with an owner heading before each group. The
    // heading rows shift every index after them, so the cursor is translated
    // through `row_of` rather than used directly.
    let groups = Grouping::of(&state.repos);
    let mut rows: Vec<Row> = Vec::with_capacity(state.repos.len() + 4);
    let mut row_of: Vec<usize> = Vec::with_capacity(state.repos.len());

    for (i, card) in state.repos.iter().enumerate() {
        if let Some(owner) = groups.heading_before(i) {
            rows.push(
                Row::new(vec![
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Line::from(vec![
                        Span::styled(owner.to_string(), Style::default().fg(theme.text_faint)),
                        Span::styled(" ╌╌╌", Style::default().fg(theme.border_dim)),
                    ])),
                ])
                // A band rather than a dashed rule doing all the work: the
                // heading is the one row that is not a repo, and a change of
                // ground says that before the eye has read anything.
                .style(Style::default().bg(mix(theme.row_idle, theme.overlay, 0.6))),
            );
        }
        row_of.push(rows.len());
        rows.push({
            // Same reasoning as the header above: until a repo is actually
            // entered, the workspace dashboard has no active repo to point at.
            let active = !state.repo_label_implicit && card.spec == state.repo_label;
            let name_style = if active {
                Style::default().fg(theme.text_bright).bold()
            } else {
                Style::default().fg(theme.text)
            };
            // Under a heading the owner is already said; repeating it on every
            // row costs the width the name needs.
            let shown = groups.short_name(&card.spec);
            let name_cell = Cell::from(Line::from(vec![
                forge_span(card, state, theme),
                Span::styled(shown, name_style),
                if active {
                    Span::styled("  ●", Style::default().fg(theme.accent))
                } else {
                    Span::raw("")
                },
            ]));

            // A repo that failed to load says so instead of pretending to be idle.
            if let Some(err) = &card.error {
                rows.push(Row::new(vec![
                    mark_cell(card),
                    Cell::from(Span::styled("!", Style::default().fg(theme.failure).bold())),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Span::styled(
                        truncate(&err.text, 46),
                        Style::default().fg(theme.failure),
                    )),
                ]
                .into_iter()
                .chain(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")))
                .collect::<Vec<_>>())
                .style(row_dress(
                    row_bg_for_status(Status::Failure, theme),
                    rows.len(),
                    i == state.repo_cursor,
                    theme,
                )));
                continue;
            }

            // A checkout with no GitHub origin never fetches, so it must not sit
            // at "loading…" forever — it just has no CI half to show.
            if !card.has_ci() {
                rows.push(Row::new(vec![
                    mark_cell(card),
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Span::styled(
                        "local only — no GitHub remote",
                        Style::default().fg(theme.text_faint).italic(),
                    )),
                ]
                .into_iter()
                .chain(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")))
                .collect::<Vec<_>>())
                .style(row_dress(
                    theme.row_idle,
                    rows.len(),
                    i == state.repo_cursor,
                    theme,
                )));
                continue;
            }

            if !card.loaded {
                rows.push(Row::new(vec![
                    mark_cell(card),
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Line::from(skeleton(14, state.tick_count, theme))),
                ]
                .into_iter()
                .chain(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")))
                .collect::<Vec<_>>())
                .style(row_dress(
                    theme.row_idle,
                    rows.len(),
                    i == state.repo_cursor,
                    theme,
                )));
                continue;
            }

            let latest = card.runs.first();
            let status = card.latest_status().unwrap_or(Status::Unknown);
            let (when_text, when_style) = latest
                .map(|r| relative_styled(r.updated_at, theme))
                .unwrap_or_else(|| ("—".into(), Style::default().fg(theme.unknown)));
            let workflow = latest
                .map(|r| truncate(&r.display_title, 28))
                .unwrap_or_else(|| "no runs".into());
            let branch = latest.map(|r| r.head_branch.clone()).unwrap_or_default();

            let (c_ok, c_fail, c_busy) = card.counts();
            let mut recent = run_sparkline(&card.runs, spark_w, theme);
            // Numbers line up down the column or they are not worth aligning at
            // all: the bar strip is padded to its full width so a repo with four
            // runs does not shift its counts left of a repo with twenty, and the
            // counts themselves are fixed-width so ✓7 and ✓20 end together.
            recent.push(Span::styled(
                format!(" ✓{c_ok:<3}"),
                Style::default().fg(theme.success_dim),
            ));
            if c_fail > 0 {
                recent.push(Span::styled(
                    format!("✗{c_fail:<3}"),
                    Style::default().fg(theme.failure),
                ));
            }
            let sparkline = Line::from(recent);
            let _ = c_busy;

            // A row whose CI moved since you last looked lights up and fades
            // back down, so the change is findable without diffing the screen
            // against your memory of it.
            let bg = match card.changed_tick {
                Some(at) => mix(
                    row_bg_for_status(status, theme),
                    style_for_status(status, theme).fg.unwrap_or(theme.text),
                    0.4 * Motion::new(state.tick_count).decay(at, FLASH_TICKS),
                ),
                None => row_bg_for_status(status, theme),
            };
            let dress = row_dress(bg, rows.len(), i == state.repo_cursor, theme);

            Row::new(vec![
                mark_cell(card),
                Cell::from(Span::styled(
                    animated_glyph(status, state.tick_count),
                    style_for_status(status, theme),
                )),
                name_cell,
                local_cell(card),
                changes_cell(card),
                Cell::from(Span::styled(workflow, Style::default().fg(theme.text))),
            ]
            .into_iter()
            .chain(cols.tail(
                ran_on_cell(card, &branch),
                // Right-aligned: "4d ago" over "130d ago" with ragged units is
                // the most reliable tell that a table was never looked at.
                Cell::from(Line::from(Span::styled(when_text, when_style)).right_aligned()),
                Cell::from(sparkline),
            ))
            .collect::<Vec<_>>())
            .style(dress)
        });
    }

    let mut widths = vec![
        Constraint::Length(1),   // batch mark
        Constraint::Length(1),   // status glyph
        Constraint::Fill(40),    // repo
        Constraint::Fill(26),    // local branch + upstream drift
        Constraint::Length(9),   // working-tree changes
        Constraint::Fill(35),    // latest workflow
    ];
    if cols.ran_on {
        widths.push(Constraint::Fill(26)); // branch the run used
    }
    if cols.updated {
        widths.push(Constraint::Length(10));
    }
    if cols.recent {
        widths.push(Constraint::Length(spark_w as u16 + 9));
    }

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        // Deliberately empty. `row_highlight_style` paints one flat style over
        // the entire selected row, so anything set here — a foreground above
        // all — overwrites the status glyph, the branch, and the clean/dirty
        // counts of the one repo you are pointing at. The selection is drawn
        // into the row's own background by `row_dress` instead; all that is
        // left for the table to do is the marker, which carries its own style.
        .row_highlight_style(Style::default())
        .highlight_symbol(Span::styled(
            "▶ ",
            Style::default().fg(theme.primary).bold(),
        ));

    let mut ts = TableState::default();
    // Through the map, not directly: every owner heading pushes the rows below
    // it down one, and selecting by repo index would highlight the wrong repo —
    // or a heading — as soon as there were two owners.
    ts.select(row_of.get(state.repo_cursor).copied());
    f.render_stateful_widget(table, inner, &mut ts);
}

/// The finished background for one dashboard row: its status tint, banded so
/// consecutive rows are told apart across the full width of the table, then
/// tinted again if it is the row under the cursor.
///
/// All three compose into a single colour rather than one painting over the
/// next, which is the whole reason the selection can stop erasing the row.
fn row_dress(base: Color, idx: usize, selected: bool, theme: &Theme) -> Style {
    let bg = banded(base, idx, theme);
    let bg = if selected {
        mix(bg, theme.select_bg, 0.5)
    } else {
        bg
    };
    Style::default().bg(bg)
}

/// Every other row lifted a hair off the one above it.
///
/// Low enough contrast to be sensed rather than seen: this is for keeping the
/// eye on one line while it travels eight columns to the right, not for drawing
/// stripes. On a 256-colour terminal `mix` cannot blend and snaps at the
/// halfway point, so a band this faint correctly amounts to nothing at all.
fn banded(bg: Color, idx: usize, theme: &Theme) -> Color {
    if idx % 2 == 1 {
        mix(bg, theme.surface_alt, 0.35)
    } else {
        bg
    }
}

/// Whether the dashboard's rows are worth grouping by owner, and where the
/// headings go.
///
/// Only when there is more than one owner: heading every row of a single-org
/// dashboard with that org says nothing and costs a row. And only when every
/// row has one — a workspace scan produces bare directory names, which have no
/// owner to group by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grouping {
    /// Owner per row, empty when grouping is off.
    owners: Vec<String>,
}

impl Grouping {
    fn of(repos: &[crate::app::state::RepoCard]) -> Self {
        let owners: Vec<String> = repos
            .iter()
            .map(|c| c.spec.split_once('/').map(|(o, _)| o.to_string()).unwrap_or_default())
            .collect();
        let distinct: std::collections::HashSet<&String> = owners.iter().collect();
        let worth_it = distinct.len() > 1 && owners.iter().all(|o| !o.is_empty());
        Self { owners: if worth_it { owners } else { Vec::new() } }
    }

    fn on(&self) -> bool {
        !self.owners.is_empty()
    }

    /// The owner heading that belongs above row `i`, if this row opens a group.
    fn heading_before(&self, i: usize) -> Option<&str> {
        let owner = self.owners.get(i)?;
        // A group opens at the first row, or wherever the owner changes.
        match i.checked_sub(1).and_then(|p| self.owners.get(p)) {
            Some(prev) if prev == owner => None,
            _ => Some(owner),
        }
    }

    /// The repo's name with the owner dropped, when a heading already carries it.
    fn short_name(&self, spec: &str) -> String {
        match self.on() {
            true => spec.split_once('/').map(|(_, r)| r.to_string()).unwrap_or_else(|| spec.into()),
            false => spec.to_string(),
        }
    }
}

/// Which of the dashboard's optional columns this terminal has room for.
///
/// Dropping whole columns rather than letting all nine shrink together: the
/// repo name and what its CI is doing are why the view exists, and eight
/// columns truncated to six characters each answers nothing. What goes first is
/// what is recoverable elsewhere — the run's branch and its age are both one
/// keystroke away in the run list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    ran_on: bool,
    updated: bool,
    recent: bool,
    /// History degrades by degrees rather than all at once, so it is the last
    /// thing to go.
    spark_w: usize,
}

impl Columns {
    fn for_width(w: u16) -> Self {
        match w {
            0..=79 => Self { ran_on: false, updated: false, recent: false, spark_w: 0 },
            80..=99 => Self { ran_on: false, updated: true, recent: false, spark_w: 0 },
            100..=119 => Self { ran_on: false, updated: true, recent: true, spark_w: 5 },
            120..=149 => Self { ran_on: true, updated: true, recent: true, spark_w: 5 },
            150..=169 => Self { ran_on: true, updated: true, recent: true, spark_w: 8 },
            _ => Self { ran_on: true, updated: true, recent: true, spark_w: 12 },
        }
    }

    /// The optional cells, in column order, keeping only what fits.
    fn tail<T>(self, ran_on: T, updated: T, recent: T) -> impl Iterator<Item = T> {
        [
            self.ran_on.then_some(ran_on),
            self.updated.then_some(updated),
            self.recent.then_some(recent),
        ]
        .into_iter()
        .flatten()
    }
}

/// The last `width` runs as a bar per run, oldest on the left.
///
/// Height is how long the run took against the longest in the window; colour is
/// how it ended. A pair of counts can say "18 passed, 2 failed" but never *"it
/// started failing three runs ago"* or *"the last green one took twice as long
/// as usual"*, which are the two questions a repo's history gets asked.
fn run_sparkline(runs: &[Run], width: usize, theme: &Theme) -> Vec<Span<'static>> {
    const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    if runs.is_empty() {
        // Padded like any other strip, so "no history" does not shove its count
        // out of the column every other row keeps it in.
        return vec![
            Span::raw(" ".repeat(width.saturating_sub(1))),
            Span::styled("—", Style::default().fg(theme.unknown)),
        ];
    }
    // `runs` is newest-first; a history reads left to right.
    let window: Vec<&Run> = runs.iter().take(width).rev().collect();
    let secs = |r: &Run| (r.updated_at - r.created_at).num_seconds().max(0) as f64;
    let longest = window.iter().copied().map(secs).fold(0.0_f64, f64::max);
    let shortest = window.iter().copied().map(secs).fold(f64::MAX, f64::min);
    // Scale across the window's own range rather than from zero, so a minute of
    // variation in ten-minute builds is still visible. But a history where every
    // run took about the same time *is* flat, and stretching a 3% spread across
    // the full height would invent a shape that isn't there.
    let spread = longest - shortest;
    let flat = longest <= 0.0 || spread / longest < 0.15;
    // A row of flat blocks is a bar chart with no time axis — every run looks
    // equally current, so the strip reads as a pattern rather than as history.
    // Fading toward the row's ground with age turns the same cells into a
    // gradient the eye reads as "recent is on the right" without being told.
    let last = window.len().saturating_sub(1).max(1) as f64;
    // Failures fade too — a red bar twenty runs back genuinely is old news —
    // but only half as far, so a bad week stays legible at the left edge.
    const FADE: f64 = 0.6;
    let mut spans: Vec<Span<'static>> = window
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let age = 1.0 - (i as f64 / last);
            let (color, fade) = match r.status {
                Status::Success => (theme.success, FADE),
                Status::Failure => (theme.failure, FADE / 2.0),
                Status::Running | Status::Queued => (theme.warning, 0.0),
                _ => (theme.unknown, FADE),
            };
            // A run with no measurable duration still gets a bar: absence of a
            // mark would read as "no run", which is a different fact.
            let h = if flat {
                3
            } else {
                (((secs(r) - shortest) / spread) * 7.0).round().clamp(0.0, 7.0) as usize
            };
            let style = Style::default().fg(mix(color, theme.row_idle, fade * age));
            Span::styled(BARS[h], style)
        })
        .collect();
    // Padded to the full column so the counts that follow start at the same x on
    // every row, whether a repo has four runs of history or twenty.
    if spans.len() < width {
        spans.insert(0, Span::raw(" ".repeat(width - spans.len())));
    }
    spans
}

/// How long a row stays lit after its CI moves — one second, which is long
/// enough to catch out of the corner of an eye and short enough that a busy
/// dashboard is not permanently glowing.
const FLASH_TICKS: u64 = 10;

/// A row that has nothing to show yet, waiting rather than broken.
///
/// A static "loading…" looks identical whether the request left half a second
/// ago or the network is gone; a moving one at least says the app is still
/// asking.
fn skeleton(width: usize, tick: u64, theme: &Theme) -> Vec<Span<'static>> {
    let head = Motion::new(tick).sweep(width, 6);
    (0..width)
        .map(|i| {
            // Weight carries the motion as well as colour, so it still reads on
            // a terminal that has flattened the palette.
            let lit = i == head || i + 1 == head;
            Span::styled(
                if lit { "━" } else { "─" },
                Style::default().fg(if lit { theme.text_faint } else { theme.text_ghost }),
            )
        })
        .collect()
}

/// The running step's name breathes between two brightnesses.
///
/// Motion is the cheapest way to say "this is now, not a snapshot" — a static
/// step name looks identical whether it started two seconds or twenty minutes
/// ago, and the whole point of the strip is that it is live.
fn breathing(tick: u64, theme: &Theme) -> Color {
    mix(theme.text, theme.text_bright, Motion::new(tick).pulse(16))
}

/// A progress bar that keeps moving even when the run doesn't.
///
/// The fill is real progress — steps a job has finished. The bright cell
/// sweeping along it is not: it exists so an eight-minute test step still reads
/// as *alive* rather than as a frozen screen.
///
/// `ratio` of `None` means nothing has started yet, so there is no honest fill.
/// A single travelling cell says "waiting" without inventing progress.
fn progress_bar(ratio: Option<f64>, width: usize, tick: u64, theme: &Theme) -> Vec<Span<'static>> {
    let head = theme.text_bright;
    let track = theme.border;
    let fill = theme.warning;
    let mut spans = Vec::with_capacity(width);
    match ratio {
        Some(r) => {
            let filled = ((r.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
            // The sweep overshoots the bar before wrapping, so there is a pause
            // between passes instead of a strobe.
            let sweep = Motion::new(tick).sweep(width, 8);
            for i in 0..width {
                let (ch, color) = if i < filled {
                    if i == sweep { ("━", head) } else { ("━", fill) }
                } else if i == filled {
                    ("╺", fill)
                } else {
                    ("─", track)
                };
                spans.push(Span::styled(ch, Style::default().fg(color)));
            }
        }
        None => {
            let pos = Motion::new(tick).bounce(width);
            for i in 0..width {
                // The weight changes with the colour, so the travel still reads
                // on a terminal that has flattened the palette.
                let (ch, color) = match (i as isize - pos as isize).abs() {
                    0 => ("━", head),
                    1 => ("━", fill),
                    _ => ("─", track),
                };
                spans.push(Span::styled(ch, Style::default().fg(color)));
            }
        }
    }
    spans
}

/// The live CI/CD strip along the bottom of the dashboard: for every repo with
/// something in flight, which workflow is running and which step it is on.
///
/// The table above can only say *that* a repo is busy. Answering "busy doing
/// what?" otherwise costs two keystrokes and a screen change per repo, which is
/// exactly the question you have while waiting on a deploy.
fn render_activity_strip(
    f: &mut Frame,
    area: Rect,
    state: &AppState,
    entries: &[(&crate::app::state::RepoCard, &crate::provider::RunDetail)],
) {
    let theme = &state.theme;
    let tick = state.tick_count;

    // A slow breath on the border, in step with the spinners: the one cue that
    // survives peripheral vision, so CI reads as alive without being read.
    let border = mix(theme.border, theme.accent_dim, Motion::new(tick).pulse(20));

    let shown = entries.len().min(area.height.saturating_sub(2) as usize);
    let mut title = vec![
        Span::styled(
            format!(" {} ", animated_glyph(Status::Running, tick)),
            Style::default().fg(theme.warning).bold(),
        ),
        Span::styled("Live", Style::default().fg(theme.text_bright).bold()),
        Span::styled(
            format!("  {} in flight ", entries.len()),
            Style::default().fg(theme.text_muted),
        ),
    ];
    // Never let the strip imply it is showing everything when it isn't.
    if shown < entries.len() {
        title.push(Span::styled(
            format!("(showing {shown}) "),
            Style::default().fg(theme.text_muted).italic(),
        ));
    }

    // What each row has to say, resolved to text before anything decides how
    // wide it may be.
    let cells: Vec<StripRow> = entries
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, (card, detail))| {
            // Entries are grouped by repo, so a second line for the same repo is
            // a second workflow on it. Dimming the repeated name says that
            // without repeating it at full weight, which reads as two rows.
            let repeat = i > 0 && entries[i - 1].0.spec == card.spec;
            let run = &detail.run;
            let running_job = detail.jobs.iter().find(|j| j.status == Status::Running);

            let (ratio, count, job, step) = match running_job {
                Some(job) => {
                    let total = job.steps.len();
                    let done = job.steps.iter().filter(|s| s.status.is_terminal()).count();
                    let cur = job.steps.iter().position(|s| s.status == Status::Running);
                    let ratio = (total > 0).then(|| done as f64 / total as f64);
                    let count = match (cur, total) {
                        (_, 0) => String::new(),
                        (Some(i), t) => format!("{}/{}", i + 1, t),
                        (None, t) => format!("{done}/{t}"),
                    };
                    let step = match cur {
                        Some(i) => StepCell::Named(job.steps[i].name.clone()),
                        None => StepCell::Note("wrapping up".into()),
                    };
                    (ratio, count, job.name.clone(), step)
                }
                // No jobs at all: GitHub has accepted the run but hasn't placed
                // it on a runner, so there is genuinely no step to name yet.
                None if detail.jobs.is_empty() => (
                    None,
                    String::new(),
                    String::new(),
                    StepCell::Note(
                        if run.status == Status::Queued {
                            "queued · waiting for a runner"
                        } else {
                            "starting up…"
                        }
                        .into(),
                    ),
                ),
                // Jobs exist but none is running: between two of them, or a
                // matrix leg is still being scheduled.
                None => {
                    let done = detail.jobs.iter().filter(|j| j.status.is_terminal()).count();
                    (
                        Some(done as f64 / detail.jobs.len() as f64),
                        format!("{done}/{} jobs", detail.jobs.len()),
                        String::new(),
                        StepCell::Note("waiting for the next job".into()),
                    )
                }
            };

            // A queued run is waiting, not stopped — the dashboard's static dot
            // would be the only still thing in a panel about motion.
            let glyph = if run.status == Status::Queued {
                const PULSE: [&str; 4] = ["·", "•", "●", "•"];
                // Quarter of the spinner's rate: waiting, not working.
                PULSE[((tick / 5) % 4) as usize].to_string()
            } else {
                animated_glyph(run.status, tick).to_string()
            };

            StripRow {
                glyph,
                repeat,
                repo: card.spec.clone(),
                workflow: run.display_title.clone(),
                branch: run.head_branch.clone(),
                job,
                step,
                ratio,
                count,
                elapsed: format_elapsed(elapsed_seconds(run)),
            }
        })
        .collect();

    // Every field gets its own column, sized to the longest one actually on
    // screen. Packing workflow·branch and job›step into two cells was cheaper,
    // but it left each row's branch, job and step at a different x — and this
    // strip is read down its columns, not across its rows. Long names now cost
    // their own column width instead of everyone else's alignment.
    let longest = |pick: &dyn Fn(&StripRow) -> usize| {
        cells.iter().map(pick).max().unwrap_or(0)
    };
    // Where a column stops growing, and where it refuses to shrink further:
    // repo, workflow, branch, job, step.
    const CAP: [usize; 5] = [28, 30, 18, 22, 48];
    const FLOOR: [usize; 5] = [10, 8, 6, 6, 12];
    let mut cols = [
        longest(&|c| disp_width(&c.repo)),
        longest(&|c| disp_width(&c.workflow)),
        longest(&|c| disp_width(&c.branch)),
        longest(&|c| disp_width(&c.job)),
        longest(&|c| c.step.width()),
    ];
    for (w, cap) in cols.iter_mut().zip(CAP) {
        *w = (*w).min(cap);
    }

    let w_count = longest(&|c| disp_width(&c.count)).min(9);
    let w_elapsed = longest(&|c| disp_width(&c.elapsed)).max(4);
    const SPACING: usize = 2;
    let inner = area.width.saturating_sub(2) as usize;
    // The bar is the first thing to go: it repeats what the counter beside it
    // already says, and on a narrow terminal the step name needs the room more.
    let bar_w = if inner >= 96 { 14 } else { 0 };
    let fixed = 1 + bar_w + w_count + w_elapsed + SPACING * 8;
    let avail = inner.saturating_sub(fixed);
    shrink_to_fit(&mut cols, &FLOOR, avail);
    // Slack goes to the step, so the elapsed clock keeps the right edge and the
    // field most likely to be cut gets whatever nobody else needed.
    cols[4] += avail.saturating_sub(cols.iter().sum::<usize>());

    let rows: Vec<Row> = cells
        .iter()
        .map(|c| {
            let step = match &c.step {
                StepCell::Named(name) => Line::from(vec![
                    Span::styled("› ", Style::default().fg(theme.text_ghost)),
                    Span::styled(
                        truncate(name, cols[4].saturating_sub(2)),
                        Style::default().fg(breathing(tick, theme)).bold(),
                    ),
                ]),
                StepCell::Note(note) => Line::from(Span::styled(
                    truncate(note, cols[4]),
                    Style::default().fg(theme.text_muted).italic(),
                )),
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    c.glyph.clone(),
                    Style::default().fg(theme.warning).bold(),
                )),
                Cell::from(Span::styled(
                    truncate(&c.repo, cols[0]),
                    if c.repeat {
                        Style::default().fg(theme.text_ghost)
                    } else {
                        Style::default().fg(theme.text_bright).bold()
                    },
                )),
                Cell::from(Span::styled(
                    truncate(&c.workflow, cols[1]),
                    Style::default().fg(theme.text),
                )),
                Cell::from(Span::styled(
                    truncate(&c.branch, cols[2]),
                    Style::default().fg(theme.accent_dim),
                )),
                Cell::from(Span::styled(
                    truncate(&c.job, cols[3]),
                    Style::default().fg(theme.text_muted),
                )),
                Cell::from(step),
                Cell::from(Line::from(progress_bar(c.ratio, bar_w, tick, theme))),
                Cell::from(Span::styled(
                    c.count.clone(),
                    Style::default().fg(theme.text_muted),
                )),
                Cell::from(
                    Line::from(Span::styled(
                        c.elapsed.clone(),
                        Style::default().fg(theme.warning),
                    ))
                    .right_aligned(),
                ),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(1), // spinner
        Constraint::Length(cols[0] as u16),
        Constraint::Length(cols[1] as u16),
        Constraint::Length(cols[2] as u16),
        Constraint::Length(cols[3] as u16),
        Constraint::Length(cols[4] as u16),
        Constraint::Length(bar_w as u16),
        Constraint::Length(w_count as u16),
        Constraint::Length(w_elapsed as u16),
    ];

    f.render_widget(
        Table::new(rows, widths).column_spacing(SPACING as u16).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border))
                .title(Line::from(title)),
        ),
        area,
    );
}

/// One line of the live strip, before anything has been measured or cut.
struct StripRow {
    glyph: String,
    /// The row above is the same repo, so the name is worth showing quietly.
    repeat: bool,
    repo: String,
    workflow: String,
    branch: String,
    /// Empty when no job is running yet — the step column says why.
    job: String,
    step: StepCell,
    ratio: Option<f64>,
    count: String,
    elapsed: String,
}

/// The step column either names the step running, or says why none is.
enum StepCell {
    Named(String),
    Note(String),
}

impl StepCell {
    /// Columns it would like, the `›` marker included.
    fn width(&self) -> usize {
        match self {
            Self::Named(s) => disp_width(s) + 2,
            Self::Note(s) => disp_width(s),
        }
    }
}

/// Take columns down from the widest end until they fit, never below `floors`.
///
/// Shrinking every column by the same share makes the short ones pay for the
/// long one; taking from whoever has the most to give keeps the narrow columns
/// whole and cuts only what was already too long to read.
fn shrink_to_fit(cols: &mut [usize], floors: &[usize], avail: usize) {
    while cols.iter().sum::<usize>() > avail {
        let Some(i) = cols
            .iter()
            .enumerate()
            .filter(|(i, w)| **w > floors[*i])
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
        else {
            return;
        };
        cols[i] -= 1;
    }
}

/// The full keybinding reference, built from the *configured* keys so remapped
/// bindings show their real values rather than the defaults.
///
/// Section titles match `View` names so the overlay can highlight wherever the
/// user currently is.
fn help_sections(km: &crate::config::KeymapConfig) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let k = |s: &str| display_key(s).to_string();
    let pair = |a: &str, b: &str| format!("{}/{}", display_key(a), display_key(b));

    vec![
        (
            "Global",
            vec![
                (pair(&km.down, &km.up), "move cursor"),
                ("↵".into(), "open / drill in"),
                (k(&km.back), "back one view"),
                (k(&km.finder), "fuzzy find in the current list"),
                (k(&km.repos_view), "multi-repo dashboard"),
                (k(&km.open_browser), "open in browser"),
                (k(&km.yank), "copy the selection to the clipboard"),
                (k(&km.help), "this help"),
                (k(&km.quit), "quit"),
            ],
        ),
        (
            "Repos — dashboard",
            vec![
                ("↵".into(), "switch to this repo"),
                (k(&km.git_view), "review local changes"),
                (k(&km.repo_mark), "mark / unmark this repo for a batch commit"),
                (k(&km.batch_commit), "commit every marked repo with one message"),
                (k(&km.open_browser), "open the repo's Actions page"),
            ],
        ),
        (
            "Batch commit",
            vec![
                (
                    "↵".into(),
                    "start — stages everything and commits each repo in turn",
                ),
                (k(&km.batch_retry), "retry the repo that failed",),
                (k(&km.batch_skip), "skip it and carry on"),
                (k(&km.git_view), "open the failed repo's working tree to fix it"),
                (pair(&km.down, &km.up), "scroll the hook output"),
                (pair(&km.next_error, &km.prev_error), "next / previous error"),
                (k(&km.git_push), "push everything the batch committed"),
                (k(&km.back), "stop — repos already committed keep their commits"),
            ],
        ),
        (
            "Changes — working tree",
            vec![
                (k(&km.git_stage), "stage / unstage the selected file"),
                (k(&km.git_stage_all), "stage all"),
                (k(&km.git_commit), "commit (opens a message prompt)"),
                (k(&km.git_push), "push — sets upstream on first push"),
                (k(&km.trigger), "open this repo's workflows to run CI"),
                (k(&km.open_browser), "open the branch's PR — or the page that creates one"),
                (k(&km.git_refresh), "re-read the working tree"),
                (format!("{}/↵", k(&km.git_diff)), "diff the selected file"),
                (
                    pair(&km.down, &km.up),
                    "scroll the hook output, while a commit/push is showing it",
                ),
                (pair(&km.next_error, &km.prev_error), "next / previous error in that output"),
                (k(&km.yank), "yank the whole hook output"),
                (k(&km.back), "dismiss the output of a failed commit/push"),
            ],
        ),
        (
            "Diff — file changes",
            vec![
                (pair(&km.down, &km.up), "scroll"),
                (pair(&km.page_down, &km.page_up), "page"),
                (pair(&km.scroll_top, &km.scroll_bottom), "top / bottom"),
                (pair(&km.next_step, &km.prev_step), "next / previous changed file"),
                (k(&km.git_stage), "stage / unstage this file"),
                (k(&km.git_refresh), "re-read the diff"),
                (k(&km.back), "back to the file list"),
            ],
        ),
        (
            "Workflows",
            vec![
                ("↵".into(), "list runs"),
                (k(&km.trigger), "trigger"),
                (k(&km.watch), "watch the latest run"),
            ],
        ),
        (
            "Runs",
            vec![
                ("↵".into(), "run detail"),
                (k(&km.trigger), "trigger"),
                (k(&km.rerun), "rerun all jobs"),
                (k(&km.rerun_failed), "rerun failed jobs"),
                (k(&km.cancel_run), "cancel"),
                (k(&km.watch), "watch"),
            ],
        ),
        (
            "Run detail",
            vec![
                (format!("↵/{}", display_key(&km.open_logs)), "open logs"),
                (k(&km.diff), "diff against the last successful run"),
            ],
        ),
        (
            "Logs",
            vec![
                (pair(&km.page_down, &km.page_up), "page down / up"),
                (pair(&km.scroll_top, &km.scroll_bottom), "top / bottom"),
                (pair(&km.next_step, &km.prev_step), "next / previous step"),
                (k(&km.all_steps), "show all steps"),
                (k(&km.search), "search (then n/p cycle matches)"),
                (pair(&km.next_error, &km.prev_error), "next / previous error"),
                (k(&km.log_focus), "focus mode — fold everything but errors"),
                ("↵".into(), "expand a fold, or collapse a group"),
            ],
        ),
        (
            "Trigger prompt",
            vec![
                (format!("↵/{}", display_key(&km.tp_edit)), "edit the field"),
                (k(&km.tp_cycle), "cycle a choice field"),
                (k(&km.tp_submit), "submit"),
                (k(&km.back), "cancel"),
            ],
        ),
    ]
}

/// Section title that corresponds to the view currently behind the overlay.
fn help_section_for(view: View) -> &'static str {
    match view {
        View::Repos => "Repos — dashboard",
        View::GitStatus => "Changes — working tree",
        View::GitDiff => "Diff — file changes",
        View::Workflows => "Workflows",
        View::Runs | View::Watch => "Runs",
        View::RunDetail | View::Diff => "Run detail",
        View::Logs => "Logs",
        View::TriggerPrompt => "Trigger prompt",
        View::BatchCommit => "Batch commit",
    }
}

/// Greedy word wrap; every returned line fits `width` (long single words get a
/// line to themselves rather than being split mid-word).
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn render_help_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    if !state.show_help {
        return;
    }
    let theme = &state.theme;
    let current = help_section_for(state.view);
    let mut sections = help_sections(&state.keymap);
    // Float the current view's section to just below Global, so "what can I do
    // here?" is answered from the top-left corner, before any reading order.
    if let Some(i) = sections.iter().position(|(t, _)| *t == current)
        && i > 1
    {
        let s = sections.remove(i);
        sections.insert(1, s);
    }

    // Key column is sized to the widest binding so descriptions line up.
    let key_w = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(6)
        .max(6);

    // Geometry before content: the column width decides where text wraps.
    // Two columns when the terminal affords them — the reference then fits on
    // one screen, which is the difference between a card you glance at and a
    // document you scroll. One column on narrow terminals rather than two
    // unreadable ones.
    const GAP: u16 = 3;
    let dialog_w = area.width.saturating_sub(4).clamp(area.width.min(46), 104);
    let inner_w = dialog_w.saturating_sub(2);
    let two_cols = inner_w >= 88;
    let col_w = if two_cols { (inner_w - GAP) / 2 } else { inner_w };
    // 1 margin + key cap (key_w + 2) + 2 gap, then the description.
    let desc_w = (col_w as usize).saturating_sub(key_w + 5).max(16);

    let build = |title: &str, rows: &[(String, &'static str)]| -> Vec<Line<'static>> {
        let is_current = title == current;
        let mut out: Vec<Line> = Vec::new();
        let mut header = vec![Span::styled(
            format!(" {title} "),
            if is_current {
                // Inverted chip: "you are here" as a mark, not a sentence.
                Style::default().bg(theme.accent).fg(theme.surface).bold()
            } else {
                Style::default().fg(theme.primary).bold()
            },
        )];
        if is_current {
            header.push(Span::styled(
                " ← you are here",
                Style::default().fg(theme.accent),
            ));
        }
        out.push(Line::from(header));
        for (key, desc) in rows {
            // The binding drawn as a key cap; the current section's caps pick
            // up the accent so the eye finds its keys first.
            let cap_style = Style::default()
                .bg(theme.surface_alt)
                .fg(if is_current { theme.accent } else { theme.text_bright })
                .bold();
            for (i, seg) in wrap_words(desc, desc_w).into_iter().enumerate() {
                if i == 0 {
                    out.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(format!(" {key:>key_w$} "), cap_style),
                        Span::raw("  "),
                        Span::styled(seg, Style::default().fg(theme.text)),
                    ]));
                } else {
                    out.push(Line::from(vec![
                        Span::raw(" ".repeat(key_w + 5)),
                        Span::styled(seg, Style::default().fg(theme.text)),
                    ]));
                }
            }
        }
        out.push(Line::default());
        out
    };

    let built: Vec<Vec<Line>> = sections.iter().map(|(t, r)| build(t, r)).collect();
    let total: usize = built.iter().map(|b| b.len()).sum();

    // Sections flow into the left column, switching to the right at the
    // boundary that leaves the two closest to level — and never switching
    // back, so reading order survives and no section splits across the fold.
    let half = total.div_ceil(2);
    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    let mut in_right = false;
    for b in built {
        if two_cols
            && !in_right
            && (left.len() + b.len()).abs_diff(half) > left.len().abs_diff(half)
        {
            in_right = true;
        }
        if in_right {
            right.extend(b);
        } else {
            left.extend(b);
        }
    }
    while left.last().is_some_and(|l| l.spans.is_empty()) {
        left.pop();
    }
    while right.last().is_some_and(|l| l.spans.is_empty()) {
        right.pop();
    }

    let content_h = left.len().max(right.len()) as u16;
    let dialog_h = (content_h + 2).min(area.height.saturating_sub(2)).max(8);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let inner_h = dialog_h.saturating_sub(2);
    let max_scroll = content_h.saturating_sub(inner_h);
    let scroll = state.help_scroll.min(max_scroll);

    let footer = if max_scroll > 0 {
        format!(
            " {}/{} scroll · {}–{} of {} · any other key closes ",
            display_key(&state.keymap.down),
            display_key(&state.keymap.up),
            scroll + 1,
            (scroll + inner_h).min(content_h),
            content_h,
        )
    } else {
        " any key closes ".to_string()
    };

    let block = Block::default()
        .title(Span::styled(
            format!(" jog v{} — keys ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.accent).bold(),
        ))
        .title_alignment(ratatui::layout::Alignment::Center)
        .title_bottom(
            Line::from(Span::styled(footer, Style::default().fg(theme.text_faint))).centered(),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent));

    let inner = block.inner(popup);
    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    if two_cols {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(col_w),
                Constraint::Length(GAP),
                Constraint::Length(col_w),
            ])
            .split(inner);
        f.render_widget(Paragraph::new(left).scroll((scroll, 0)), cols[0]);
        f.render_widget(Paragraph::new(right).scroll((scroll, 0)), cols[2]);
    } else {
        f.render_widget(Paragraph::new(left).scroll((scroll, 0)), inner);
    }
}

fn render_git_status(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some(gv) = state.git_view.as_ref() else {
        f.render_widget(
            Paragraph::new("(no repo selected)").block(styled_block("Changes", theme)),
            area,
        );
        return;
    };

    let title = match &gv.status {
        Some(s) => {
            let mut t = format!("{} — {}", gv.spec, s.branch);
            if s.ahead > 0 {
                t.push_str(&format!("  ↑{}", s.ahead));
            }
            if s.behind > 0 {
                t.push_str(&format!("  ↓{}", s.behind));
            }
            if !s.has_upstream {
                t.push_str("  (no upstream)");
            }
            t
        }
        None => format!("{} — loading…", gv.spec),
    };

    let blk = styled_block(&title, theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    if inner.height < 2 {
        return;
    }

    // A running or failed command takes the bottom half. It is capped rather
    // than given a fixed share so a two-line failure doesn't push the file list
    // off screen, and a 2,000-line pytest run doesn't get four rows.
    let op = state.current_op();
    let op_height = op.map(|op| {
        let wanted = op.lines.len() as u16 + 3; // header + borders
        wanted.clamp(6, (inner.height / 2).max(6)).min(inner.height.saturating_sub(4))
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(match op_height {
            Some(h) => vec![
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(h),
            ],
            None => vec![Constraint::Length(2), Constraint::Min(0)],
        })
        .split(inner);

    if let (Some(op), Some(area)) = (op, chunks.get(2)) {
        render_op_output(f, *area, op, state);
    }

    // ── Summary line ───────────────────────────────────────────────────
    let staged = gv.staged_count();
    let unstaged = gv
        .status
        .as_ref()
        .map(|s| s.unstaged_count())
        .unwrap_or(0);
    let summary = if gv.status.is_none() {
        Line::from(Span::styled(
            "reading working tree…",
            Style::default().fg(theme.text_faint),
        ))
    } else if gv.entries().is_empty() {
        // "Nothing to commit" is only the whole truth when there is also
        // nothing to push — a clean tree sitting ahead of upstream is mid-task,
        // and this line is the only place the view says what the next step is.
        let ahead = gv.status.as_ref().map(|s| s.ahead).unwrap_or(0);
        if ahead > 0 {
            Line::from(vec![
                Span::styled("✓ clean", Style::default().fg(theme.success).bold()),
                Span::styled(
                    format!(
                        "   ↑{ahead} commit{} not pushed",
                        if ahead == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(theme.accent).bold(),
                ),
                Span::styled(
                    format!("   {} pushes", state.keymap.git_push),
                    Style::default().fg(theme.text_muted),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("✓ clean", Style::default().fg(theme.success).bold()),
                Span::styled(
                    "   nothing to commit",
                    Style::default().fg(theme.text_muted),
                ),
            ])
        }
    } else {
        Line::from(vec![
            Span::styled(
                format!("{staged} staged"),
                if staged > 0 {
                    Style::default().fg(theme.success).bold()
                } else {
                    Style::default().fg(theme.text_muted)
                },
            ),
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("{unstaged} unstaged"),
                Style::default().fg(theme.warning),
            ),
            Span::styled(
                format!("   {}", gv.path.display()),
                Style::default().fg(theme.text_faint),
            ),
        ])
    };
    // The branch's PR, on the same glance as the branch itself. Only when one
    // exists: "no PR" is not information anyone came here for, and the footer
    // already says which key would make one.
    let mut summary = summary;
    if let Some(pr) = gv.open_pr() {
        summary.spans.push(Span::styled(
            format!("   PR #{}", pr.number),
            Style::default().fg(theme.accent).bold(),
        ));
        summary.spans.push(Span::styled(
            format!(" {}", truncate(&pr.title, 36)),
            Style::default().fg(theme.text_muted),
        ));
        if pr.draft {
            summary.spans.push(Span::styled(
                " · draft",
                Style::default().fg(theme.text_faint),
            ));
        }
    }
    f.render_widget(Paragraph::new(summary), chunks[0]);

    // ── File list ──────────────────────────────────────────────────────
    let rows: Vec<Row> = gv
        .entries()
        .iter()
        .map(|e| {
            let staged = e.is_staged();
            let mark = if staged && e.has_unstaged() {
                "◐"
            } else if staged {
                "●"
            } else {
                "○"
            };
            let mark_style = if staged {
                Style::default().fg(theme.success).bold()
            } else {
                Style::default().fg(theme.text_faint)
            };
            let path_style = if staged {
                Style::default().fg(theme.text_bright)
            } else {
                Style::default().fg(theme.text_muted)
            };
            let label_style = match e.label() {
                "deleted" => Style::default().fg(theme.failure),
                "untracked" => Style::default().fg(theme.text_muted),
                "conflict" => Style::default().fg(theme.failure).bold(),
                _ => Style::default().fg(theme.warning),
            };
            Row::new(vec![
                Cell::from(Span::styled(mark, mark_style)),
                Cell::from(Span::styled(e.code(), Style::default().fg(theme.text_muted))),
                Cell::from(Span::styled(e.label(), label_style)),
                Cell::from(Span::styled(e.path.clone(), path_style)),
            ])
        })
        .collect();

    if rows.is_empty() {
        return;
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),   // staged marker
            Constraint::Length(2),   // XY code
            Constraint::Length(10),  // label
            Constraint::Fill(1),     // path
        ],
    )
    .header(
        Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(Span::styled("Change", Style::default().fg(theme.text_muted))),
            Cell::from(Span::styled("File", Style::default().fg(theme.text_muted))),
        ])
        .height(1)
        .bottom_margin(1),
    )
    .column_spacing(1)
    .row_highlight_style(
        Style::default()
            .bg(theme.select_bg)
            .fg(theme.text_bright)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    let mut ts = TableState::default();
    ts.select(Some(gv.cursor));
    f.render_stateful_widget(table, chunks[1], &mut ts);
}

/// One dashboard row's note that this repo has a commit or push in it.
///
/// Deliberately terse: it shares a proportional column with the branch name,
/// where a long name plus "pre-commit hook" would clip whichever came second.
/// Which hook it was is in the pane, one keypress away; the row only has to say
/// that this repo is in a commit, and whether that commit is now stuck.
fn op_row_marker(op: &GitOp, tick: u64, theme: &Theme) -> Span<'static> {
    if op.finished {
        Span::styled(
            format!("  ✗ {}", op.verb),
            Style::default().fg(theme.failure).bold(),
        )
    } else {
        Span::styled(
            format!("  {} {}", animated_glyph(Status::Running, tick), op.verb),
            Style::default().fg(theme.warning),
        )
    }
}

/// The output of a running — or just-failed — commit or push.
///
/// This pane exists for hooks. A `pre-commit` running pytest or pyright is the
/// difference between a commit taking 40 milliseconds and 40 seconds, and while
/// it runs the view would otherwise sit there looking hung. When it fails, its
/// output is the only account of *why* anywhere in the system: git exits
/// non-zero and adds nothing.
fn render_op_output(f: &mut Frame, area: Rect, op: &GitOp, state: &AppState) {
    let theme = &state.theme;
    let errors = op.error_count();
    let (title, border) = if !op.finished {
        let glyph = Motion::new(state.tick_count).spinner_slow();
        (
            format!(
                "{glyph} {} · {}s",
                op.label(),
                op.elapsed_secs(state.tick_count)
            ),
            theme.warning,
        )
    } else if op.failed {
        let tally = if errors > 0 {
            format!(" · {errors}✗")
        } else {
            String::new()
        };
        (format!("✗ {} failed{tally}", op.label()), theme.failure)
    } else {
        (format!("✓ {}", op.label()), theme.success)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(border).bold(),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let viewport = inner.height as usize;
    state.last_op_viewport_height.set(inner.height);
    let offset = op.scroll_offset(viewport);

    let mut lines: Vec<Line> = Vec::with_capacity(viewport);
    // Say so when the head of a long run was dropped, rather than let the top
    // of the pane read as the start of the output.
    if offset == 0 && op.dropped > 0 {
        lines.push(Line::from(Span::styled(
            format!("⋯ {} earlier lines dropped ⋯", op.dropped),
            Style::default().fg(theme.text_muted),
        )));
    }
    for l in op.lines.iter().skip(offset).take(viewport - lines.len()) {
        let style = if l.error {
            Style::default().fg(theme.failure)
        } else if l.warn {
            Style::default().fg(theme.warning)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(Span::styled(l.text.clone(), style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "waiting for output…",
            Style::default().fg(theme.text_muted),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// One commit message going out across several repos, one repo at a time.
///
/// Separate from the per-repo Changes view on purpose: this is a queue you are
/// supervising, so what matters is where it has got to, what it did to each
/// repo, and — when a hook says no — the output that explains it, alone on the
/// screen rather than three repos deep in a scrollback.
fn render_batch_commit(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some(batch) = state.batch.as_ref() else {
        return;
    };

    // The op pane only earns its space while there is something to show in it.
    let op = batch
        .current()
        .filter(|_| batch.phase != BatchPhase::Compose)
        .and_then(|i| state.git_ops.get(&i.spec));
    // The queue never squeezes the bottom pane out: a hook you cannot read, or
    // a message box you have to type into blind because a dozen marked repos
    // filled the screen, are the two things this view exists to avoid.
    let composing = batch.input.is_some();
    let lower = op.is_some() || composing;
    // Borders, a row of top padding, and four lines of prompt. Fixed, because a
    // four-line prompt stretched down forty rows is its own kind of clutter.
    const COMPOSE_H: u16 = 7;
    let reserve = if composing { COMPOSE_H } else { 3 };
    let room = if lower {
        area.height.saturating_sub(reserve + 1).max(3)
    } else {
        area.height
    };
    let list_h = (batch.items.len() as u16 + 2).min(room);
    // A blank row between the two panes. Stacked borders touching each other
    // read as one dense box; the gap is what makes them two things.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(match (lower, composing) {
            // The hook output takes everything left: it is unbounded, and the
            // line you need is as likely to be the last one as the first.
            (true, false) => [
                Constraint::Length(list_h),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(0),
            ],
            (true, true) => [
                Constraint::Length(list_h),
                Constraint::Length(1),
                Constraint::Length(COMPOSE_H.min(area.height.saturating_sub(list_h + 1))),
                Constraint::Min(0),
            ],
            _ => [
                Constraint::Length(list_h),
                Constraint::Length(0),
                Constraint::Min(0),
                Constraint::Length(0),
            ],
        })
        .split(area);
    let (top, bottom) = (chunks[0], chunks[2]);

    let (title, border) = match batch.phase {
        BatchPhase::Compose => (
            format!(" Commit {} repo{} ", batch.items.len(), plural(batch.items.len())),
            theme.accent,
        ),
        BatchPhase::Paused => (
            format!(
                " Paused on {} — {} retry · {} skip · {} open · Esc stop ",
                batch.current().map(|i| i.spec.as_str()).unwrap_or("?"),
                display_key(&state.keymap.batch_retry),
                display_key(&state.keymap.batch_skip),
                display_key(&state.keymap.git_view),
            ),
            theme.failure,
        ),
        BatchPhase::AskPush => {
            let t = batch.tally();
            (
                format!(
                    " {} committed — {} pushes them all, Esc finishes ",
                    t.committed,
                    display_key(&state.keymap.git_push),
                ),
                theme.warning,
            )
        }
        BatchPhase::Done => {
            let t = batch.tally();
            let mut parts = vec![format!("{} committed", t.committed)];
            if t.pushed > 0 {
                parts.push(format!("{} pushed", t.pushed));
            }
            if t.failed > 0 {
                parts.push(format!("{} failed", t.failed));
            }
            if t.nothing > 0 {
                parts.push(format!("{} had nothing to do", t.nothing));
            }
            if t.untouched > 0 {
                parts.push(format!("{} not attempted", t.untouched));
            }
            (
                format!(" Done · {} ", parts.join(" · ")),
                if t.failed > 0 { theme.failure } else { theme.success },
            )
        }
        _ => (
            format!(
                " {} {} · {}/{} ",
                animated_glyph(Status::Running, state.tick_count),
                if batch.phase == BatchPhase::Pushing { "Pushing" } else { "Committing" },
                batch.cursor + 1,
                batch.items.len(),
            ),
            theme.warning,
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .padding(Padding::horizontal(1))
        .title(Span::styled(title, Style::default().fg(border).bold()));
    let inner = block.inner(top);
    f.render_widget(block, top);

    let dim = Style::default().fg(theme.text_muted);
    // With more repos than rows, follow the one being worked on — a queue that
    // scrolled the live repo off the top would be worse than no list at all.
    let rows = inner.height as usize;
    let hidden = batch.items.len().saturating_sub(rows);
    let first = if hidden == 0 {
        0
    } else {
        // One row of the window is spent saying what it is hiding.
        batch.cursor.saturating_sub(1).min(hidden + 1)
    };
    let mut lines: Vec<Line> = batch
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(rows.saturating_sub(usize::from(hidden > 0)))
        .map(|(i, item)| {
            let running = item.state == ItemState::Running && batch.is_working();
            let (glyph, gstyle, note, nstyle) = match &item.state {
                ItemState::Queued => (
                    "·",
                    dim,
                    if batch.phase == BatchPhase::Done {
                        "not attempted".to_string()
                    } else {
                        "queued".to_string()
                    },
                    dim,
                ),
                ItemState::Running => (
                    animated_glyph(Status::Running, state.tick_count),
                    Style::default().fg(theme.warning).bold(),
                    format!(
                        "{}… {}s",
                        if batch.phase == BatchPhase::Pushing { "pushing" } else { "committing" },
                        state.tick_count.saturating_sub(batch.started_tick) / 10,
                    ),
                    Style::default().fg(theme.text_bright),
                ),
                ItemState::Committed => (
                    "✓",
                    Style::default().fg(theme.success),
                    format!("committed {}", item.sha.as_deref().unwrap_or("HEAD")),
                    Style::default().fg(theme.success_dim),
                ),
                ItemState::Pushed => (
                    "✓",
                    Style::default().fg(theme.success).bold(),
                    format!("pushed {}", item.sha.as_deref().unwrap_or("HEAD")),
                    Style::default().fg(theme.success),
                ),
                // A push with nothing to do still leaves the batch's commit on
                // disk; saying only "already up to date" would read as untouched.
                ItemState::Nothing(why) => match item.sha.as_deref() {
                    Some(sha) => (
                        "✓",
                        Style::default().fg(theme.success),
                        format!("committed {sha} · {why}"),
                        Style::default().fg(theme.success_dim),
                    ),
                    None => ("–", dim, why.clone(), dim),
                },
                // A push that failed still reports the commit that landed:
                // "failed" alone would read as "this repo is untouched".
                ItemState::Failed(err) => (
                    "✗",
                    Style::default().fg(theme.failure).bold(),
                    match item.sha.as_deref() {
                        Some(sha) => format!("committed {sha}, then failed: {err}"),
                        None => err.clone(),
                    },
                    Style::default().fg(theme.failure),
                ),
            };
            let name_style = if running || (batch.phase == BatchPhase::Paused && i == batch.cursor) {
                Style::default().fg(theme.text_bright).bold()
            } else {
                Style::default().fg(theme.text)
            };
            Line::from(vec![
                Span::styled(format!("{glyph}  "), gstyle),
                Span::styled(format!("{:<24}", truncate(&item.spec, 24)), name_style),
                Span::styled(truncate(&note, inner.width.saturating_sub(30) as usize), nstyle),
            ])
        })
        .collect();
    if hidden > 0 {
        let shown = lines.len();
        let t = batch.tally();
        lines.push(Line::from(Span::styled(
            format!(
                "… {} more · {} committed · {} failed · {} waiting",
                batch.items.len() - shown,
                t.committed,
                t.failed,
                t.untouched,
            ),
            dim,
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);

    match (batch.input.as_ref(), op) {
        // Composing: the message box sits where the output will be, so the eye
        // does not have to move once the run starts.
        (Some(buf), _) => {
            let dirty: usize = batch
                .items
                .iter()
                .filter_map(|i| state.repos.iter().find(|c| c.spec == i.spec))
                .filter_map(|c| c.git.as_ref())
                .map(|g| g.entries.len())
                .sum();
            let blk = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.accent))
                // The message is the one thing being typed on this screen, so it
                // gets room around it rather than sitting against the border.
                .padding(Padding::new(2, 2, 1, 0))
                .title(Span::styled(
                    format!(
                        " One message for {} repo{} · {dirty} changed file{} ",
                        batch.items.len(),
                        plural(batch.items.len()),
                        plural(dirty),
                    ),
                    Style::default().fg(theme.accent).bold(),
                ));
            let bi = blk.inner(bottom);
            f.render_widget(blk, bottom);
            // The caret line, then a gap, then the notes — so the eye lands on
            // what it is typing instead of on a paragraph of hints.
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(buf.as_str(), Style::default().fg(theme.text_bright)),
                    Span::styled("█", Style::default().fg(theme.accent)),
                ]),
                Line::raw(""),
            ];
            // On a short pane the caret comes first and the notes go without.
            if bi.height >= 4 {
                lines.push(Line::from(Span::styled(
                    "every repo above is staged with `git add -A` and committed with this message",
                    dim,
                )));
            }
            lines.push(Line::from(Span::styled("↵ start · Esc cancel", dim)));
            f.render_widget(Paragraph::new(lines), bi);
        }
        (None, Some(op)) => render_op_output(f, bottom, op, state),
        (None, None) => {}
    }
}

/// Style one line of diff text.
///
/// Split out so the classification is testable, and because the `+++`/`---`
/// file headers are the easy thing to get wrong: they start with the same
/// characters as content but are not additions or deletions.
fn diff_line_style(text: &str, theme: &Theme) -> Style {
    let meta = Style::default().fg(theme.text_muted);
    if text.starts_with("+++") || text.starts_with("---") {
        return meta;
    }
    match text.chars().next() {
        Some('+') => Style::default().fg(theme.success),
        Some('-') => Style::default().fg(theme.failure),
        Some('@') if text.starts_with("@@") => Style::default().fg(theme.accent).bold(),
        // `diff --git`, `index abc..def`, `new file mode …`, `Binary files …`
        _ if text.starts_with("diff ")
            || text.starts_with("index ")
            || text.starts_with("new file")
            || text.starts_with("deleted file")
            || text.starts_with("old mode")
            || text.starts_with("new mode")
            || text.starts_with("similarity index")
            || text.starts_with("rename ")
            || text.starts_with("Binary files") =>
        {
            meta
        }
        _ => Style::default().fg(theme.text),
    }
}

/// The diff for one file from the working-tree view.
fn render_git_diff(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    // Inner area = area minus the rounded border (1 row/col on each side).
    let viewport = area.height.saturating_sub(2);
    state.last_diff_viewport_height.set(viewport);

    let Some(dv) = state.git_diff.as_ref() else {
        f.render_widget(
            Paragraph::new("(no file selected)").block(styled_block("Diff", theme)),
            area,
        );
        return;
    };

    let (add, del) = dv.stats();
    let mut title = format!("{}  ", dv.file);
    if dv.loading {
        title.push_str("loading…");
    } else {
        title.push_str(&format!("+{add} −{del}"));
    }

    // Position, so a long diff says how much of it is off-screen.
    let total = dv.lines.len();
    if total > viewport as usize {
        let last = (dv.scroll + viewport as usize).min(total);
        title.push_str(&format!("   [{}–{} of {}]", dv.scroll + 1, last, total));
    }

    if dv.lines.is_empty() {
        let msg = if dv.loading {
            "reading diff…"
        } else {
            // Mode changes and pure renames are real status entries with no
            // textual diff at all — say so rather than showing a blank pane.
            "no textual changes (mode change, rename, or an empty file)"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(theme.text_faint).italic()))
                .block(styled_block(&title, theme)),
            area,
        );
        return;
    }

    // Slicing to the viewport keeps the per-frame clone bounded rather than
    // copying a 10k-line diff on every redraw.
    let first = dv.scroll.min(total);
    let lines: Vec<Line> = dv.lines[first..]
        .iter()
        .take(viewport as usize)
        .map(|l| match l {
            crate::app::state::DiffLine::Section(label) => Line::from(Span::styled(
                format!("── {label} "),
                Style::default().fg(theme.accent).bold(),
            )),
            crate::app::state::DiffLine::Text(t) => {
                Line::from(Span::styled(t.clone(), diff_line_style(t, theme)))
            }
        })
        .collect();

    // Deliberately not wrapped: a wrapped hunk reflows every line below it, so
    // a long line would push the rest of the diff out of alignment with the
    // scroll offset — the same reason the log view slices instead of wrapping.
    f.render_widget(
        Paragraph::new(lines).block(styled_block(&title, theme)),
        area,
    );
}

/// Commit message prompt, drawn over the working-tree view.
fn render_commit_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some(buf) = state
        .git_view
        .as_ref()
        .and_then(|g| g.commit_input.as_ref())
    else {
        return;
    };
    let staged = state.git_view.as_ref().map(|g| g.staged_count()).unwrap_or(0);

    let dialog_w = (area.width * 70 / 100).max(40).min(area.width);
    let dialog_h = 4u16.min(area.height);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let accent = state.theme.accent;
    let block = Block::default()
        .title(Span::styled(
            format!(" Commit {staged} staged file{} ", if staged == 1 { "" } else { "s" }),
            Style::default().fg(accent).bold(),
        ))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(popup);

    let lines = vec![
        Line::from(vec![
            Span::styled("  ", Style::default().fg(accent)),
            Span::styled(buf.as_str(), Style::default().fg(theme.text_bright)),
            Span::styled("█", Style::default().fg(accent)),
        ]),
        Line::from(Span::styled(
            "  ↵ commit · Esc cancel",
            Style::default().fg(theme.text_faint),
        )),
    ];

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(Paragraph::new(lines), inner);
}

/// The push question, centred over whatever raised it.
///
/// Two buttons rather than a key hint: the answer is a decision, and a decision
/// wants something to point at. Yes is pre-selected, so the whole exchange is
/// one Enter — the same key that finished the commit message a moment ago.
fn render_push_prompt(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some(p) = state.push_prompt.as_ref() else {
        return;
    };

    let dialog_w = 46u16.min(area.width);
    let dialog_h = 6u16.min(area.height);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let accent = theme.accent;
    let block = Block::default()
        .title(Span::styled(" Push? ", Style::default().fg(accent).bold()))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(popup);

    // A first push creates the remote branch. That is a different act from
    // adding to one that exists, and worth saying before it happens.
    let what = if let Some(n) = p.batch_count {
        format!(
            "{n} repo{} committed — push them all?",
            if n == 1 { "" } else { "s" }
        )
    } else if p.has_upstream {
        format!("committed — push {} to origin?", p.branch)
    } else {
        format!("committed — publish {} to origin?", p.branch)
    };
    let button = |label: &str, selected: bool| {
        if selected {
            Span::styled(
                format!("  {label}  "),
                Style::default().bg(accent).fg(theme.surface).bold(),
            )
        } else {
            Span::styled(format!("  {label}  "), Style::default().fg(theme.text_muted))
        }
    };

    let lines = vec![
        Line::from(Span::styled(
            truncate(&what, inner.width as usize),
            Style::default().fg(theme.text_bright),
        ))
        .centered(),
        Line::from(""),
        Line::from(vec![
            button("Yes", p.yes),
            Span::raw("   "),
            button("No", !p.yes),
        ])
        .centered(),
        Line::from(Span::styled(
            "↵ take it · ←/→ switch · Esc not now",
            Style::default().fg(theme.text_faint),
        ))
        .centered(),
    ];

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(Paragraph::new(lines), inner);
}

/// Cut `s` to `max` characters, adding an ellipsis when it was longer.
/// `s` for anything but one. Counts read as sloppy the moment they say
/// "1 repos", and this view is nothing but counts.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How many terminal columns a string occupies.
///
/// Not its length in `char`s: workflow names carry emoji, and every one of them
/// is two columns wide. Measured by count, a name with two emoji in it overruns
/// the column it was cut to fit — which is how a branch beside one went missing
/// from the live strip while the same field on the row above was fine.
fn disp_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// `s` cut to at most `max` terminal columns, with an ellipsis where it was cut.
fn truncate(s: &str, max: usize) -> String {
    if disp_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // The ellipsis takes a column of its own, so the text gets one fewer.
    let budget = max - 1;
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn render_finder_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some(finder) = &state.finder else { return };

    let dialog_w = (area.width * 70 / 100).max(40).min(area.width);
    let list_rows = finder.matches.len().clamp(1, 12) as u16;
    // query line + separator + results, inside a border.
    let dialog_h = (list_rows + 4).min(area.height);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 3;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let accent = state.theme.accent;
    let label = match finder.kind {
        crate::app::state::FinderKind::Repos => "Find repo",
        crate::app::state::FinderKind::Workflows => "Find workflow",
        crate::app::state::FinderKind::Runs => "Find run",
        crate::app::state::FinderKind::DetailItems => "Find job / step",
        crate::app::state::FinderKind::GitEntries => "Find changed file",
    };
    let block = Block::default()
        .title(Span::styled(
            format!(" {label}  ({}/{}) ", finder.matches.len(), finder.items.len()),
            Style::default().fg(accent).bold(),
        ))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(popup);

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("  ", Style::default().fg(accent)),
            Span::styled(finder.query.as_str(), Style::default().fg(theme.text_bright)),
            Span::styled("█", Style::default().fg(accent)),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(theme.border),
        )),
    ];

    if finder.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(theme.text_faint).italic(),
        )));
    }

    // Scroll the visible window so the cursor stays on screen in long lists.
    let visible = list_rows as usize;
    let start = finder.cursor.saturating_sub(visible.saturating_sub(1));
    for (row, &item_idx) in finder.matches.iter().enumerate().skip(start).take(visible) {
        let selected = row == finder.cursor;
        let label = finder
            .items
            .get(item_idx)
            .map(|(_, l)| l.as_str())
            .unwrap_or("");
        let style = if selected {
            Style::default().fg(theme.text_bright).bold()
        } else {
            Style::default().fg(theme.text_muted)
        };
        let line = Line::from(vec![
            Span::styled(if selected { "▶ " } else { "  " }, Style::default().fg(accent)),
            Span::styled(truncate(label, inner.width.saturating_sub(3) as usize), style),
        ]);
        lines.push(if selected {
            line.style(Style::default().bg(theme.select_bg))
        } else {
            line
        });
    }

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(Paragraph::new(lines), inner);
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
    let mut tallies = vec![
        Span::styled(format!("✓{wf_ok}"), Style::default().fg(theme.success)),
        Span::styled(format!("  ✗{wf_fail}"), Style::default().fg(theme.failure)),
    ];
    if wf_run > 0 {
        tallies.push(Span::styled(
            format!("  ⏵{wf_run}"),
            Style::default().fg(theme.warning).bold(),
        ));
    }
    let blk = panel(&format!("Workflows  {count}"), tallies, theme, theme.primary);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    let sel_bg = theme.select_bg;
    let sel_fg = theme.text_bright;
    let hdr = Style::default().fg(theme.text_muted);

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
                .map(|t| relative_styled(t.with_timezone(&Utc), theme))
                .unwrap_or_else(|| ("—".into(), Style::default().fg(theme.unknown)));
            let trig = if w.triggerable { "t" } else { " " };

            let row_bg = row_bg_for_status(status, theme);
            Row::new(vec![
                Cell::from(Span::styled(animated_glyph(status, state.tick_count), style_for_status(status, &state.theme))),
                Cell::from(Span::styled(w.name.clone(), Style::default())),
                Cell::from(Span::styled(
                    w.file_name.clone(),
                    Style::default().fg(theme.text_muted),
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
    if state.workflows.is_empty() {
        render_empty(
            f,
            inner,
            theme,
            "⚙",
            "This repo has no workflows.",
            "add one under .github/workflows, or press H for the other repos",
        );
        return;
    }
    ts.select(Some(state.workflow_cursor));
    f.render_stateful_widget(table, inner, &mut ts);
}

fn render_workflows_preview(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
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
            Paragraph::new(Span::styled("loading…", Style::default().fg(theme.text_faint))),
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

    let sel_bg = theme.select_bg_dim;
    let sel_fg = theme.text_bright;
    let hdr = Style::default().fg(theme.text_muted);

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
            let (when_text, when_style) = relative_styled(r.updated_at, theme);
            Row::new(vec![
                Cell::from(Span::styled(animated_glyph(r.status, state.tick_count), style_for_status(r.status, &state.theme))),
                Cell::from(Span::styled(r.head_branch.clone(), Style::default().fg(theme.accent))),
                Cell::from(Span::styled(when_text, when_style)),
            ])
            .style(Style::default().bg(row_bg_for_status(r.status, theme)))
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
    let mut tallies = vec![
        Span::styled(format!("✓{ok}"), Style::default().fg(theme.success)),
        Span::styled(format!("  ✗{fail}"), Style::default().fg(theme.failure)),
    ];
    if running > 0 {
        tallies.push(Span::styled(
            format!("  ⏵{running}"),
            Style::default().fg(theme.warning).bold(),
        ));
    }
    let name = if state.workflow_for_runs.is_some() {
        format!("Runs — {}  {}", wf_label, state.runs.len())
    } else {
        format!("Runs  {}", state.runs.len())
    };
    let blk = panel(&name, tallies, theme, theme.primary)
        .style(Style::default().bg(theme.overlay));
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    let sel_bg = theme.select_bg_dim;
    let sel_fg = theme.text_bright;
    let hdr = Style::default().fg(theme.text_muted);

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
            let (when_text, when_style) = relative_styled(r.updated_at, theme);
            let dur_secs = elapsed_seconds(r);
            let dur_text = format_elapsed(dur_secs);
            let dur_style = if dur_secs > 900 {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.text_muted)
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
                    Span::styled("⎿ ", Style::default().fg(theme.text_ghost)),
                    Span::styled(msg, Style::default().fg(theme.text_muted)),
                ])
            };
            let branch_cell = ratatui::text::Text::from(vec![
                Line::from(Span::styled(r.head_branch.clone(), Style::default().fg(theme.accent))),
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
    if state.runs.is_empty() {
        render_empty(
            f,
            inner,
            theme,
            "◷",
            "This workflow has never run.",
            &format!("{} triggers it", display_key(&state.keymap.trigger)),
        );
        return;
    }
    ts.select(Some(state.run_cursor));
    f.render_stateful_widget(table, inner, &mut ts);
}

fn render_runs_preview(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
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
            Paragraph::new(Span::styled("loading…", Style::default().fg(theme.text_faint))),
            inner,
        );
        return;
    }

    if let Some(detail) = &state.runs_preview {
        let mut lines: Vec<Line> = Vec::new();

        if let Some(run) = selected
            && !run.commit_msg.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("󰊢 ", Style::default().fg(theme.text_muted)),
                    Span::styled(run.commit_msg.clone(), Style::default().fg(theme.text).italic()),
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
    let theme = &state.theme;
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
    let sel_bg = theme.surface_alt;

    let rows: Vec<Row> = items.iter().enumerate().map(|(flat_idx, item)| {
        let selected = flat_idx == cursor;
        let row_style = if selected { Style::default().bg(sel_bg) } else { Style::default() };
        match item {
            DetailItem::Job(ji) => {
                let job = &detail.jobs[*ji];
                let prefix = if selected { "▶ " } else { "  " };
                let name_style = if selected {
                    Style::default().bold().fg(theme.primary)
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
                    Style::default().fg(theme.text_bright).bold()
                } else {
                    Style::default().fg(theme.text_muted)
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
                        .fg.unwrap_or(theme.text_faint);
                    (
                        Cell::from(format!("{dur:>6}")).style(Style::default().fg(theme.text_ghost)),
                        Cell::from(bar).style(Style::default().fg(bar_color)),
                    )
                } else {
                    (Cell::from(""), Cell::from(""))
                };
                let badge_cell = if let Some((failed, total)) = stats.get(&step.name).copied()
                    && failed > 0
                {
                    let s = if failed * 2 >= total {
                        Style::default().fg(theme.failure).bold()
                    } else {
                        Style::default().fg(theme.accent)
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
        Span::styled(format!("  ⏱ {dur}"), Style::default().fg(theme.text_muted)),
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
    // Inner area = area minus the rounded border (1 row/col on each side).
    let viewport = area.height.saturating_sub(2);
    state.last_logs_viewport_height.set(viewport);
    state.last_logs_viewport_width.set(area.width.saturating_sub(2));

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

    if state.log_focus {
        // In focus mode the raw line count is a lie — report what's on screen.
        log_title.push_str(&format!(
            "   focus  ({} of {} lines · {}✗ {}⚠)",
            // Fold markers are not log lines; counting them would overstate how
            // much of the log survived the filter.
            state.log_rendered.len() - state.log_fold_rows.len(),
            state.log_lines.len(),
            state.log_error_lines.len(),
            state.log_warn_lines.len(),
        ));
    } else {
        // The error count belongs in the title, not just behind an `e` press:
        // "is there anything wrong in here" is the first question a log has to
        // answer, and scrolling to find out is the slow way to ask it.
        log_title.push_str(&format!("  ({} lines", state.log_lines.len()));
        if !state.log_error_lines.is_empty() {
            log_title.push_str(&format!(" · {}✗", state.log_error_lines.len()));
        }
        if !state.log_warn_lines.is_empty() {
            log_title.push_str(&format!(" · {}⚠", state.log_warn_lines.len()));
        }
        log_title.push(')');
    }

    // `log_scroll` is an index into `log_rendered`, not a visual row offset:
    // we slice from it and let the widget draw from the top. Scrolling a
    // wrapping Paragraph instead would need the visual height of every line
    // above the viewport, which can only be estimated — and the error compounds
    // over a long log, so jumps land in the wrong place.
    let first = state.log_scroll.min(state.log_rendered.len());
    // Every rendered line occupies at least one screen row, so `viewport` of
    // them is all the widget can possibly draw. Cutting there keeps the per-frame
    // clone bounded instead of copying the tail of a 40k-line log each redraw.
    // Cursor and current-match highlighting are applied here rather than during
    // the rebuild, so moving the cursor costs a viewport, not the whole buffer.
    // The map needs a column of its own; text running into it would read as
    // corruption rather than as a second thing on the screen.
    let mapped = minimap_fits(area, state.log_rendered.len(), viewport as usize);
    if mapped {
        state.last_logs_viewport_width.set(area.width.saturating_sub(4));
    }
    let mut blk = styled_block(&log_title, &state.theme);
    if mapped {
        blk = blk.padding(Padding::right(2));
    }
    let p = Paragraph::new(state.decorate_visible(first, viewport as usize))
        .block(blk)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
    if mapped {
        render_log_minimap(f, area, state, first, viewport as usize);
    }
}

/// Whether a log is long enough, and a pane wide enough, for a map to say
/// anything. Below a few screens everything is one page away.
fn minimap_fits(area: Rect, total: usize, viewport: usize) -> bool {
    area.height >= 6 && area.width >= 30 && total > viewport.max(1) * 2
}

/// A one-column map of the whole log down the right-hand rule.
///
/// The viewport shows a few dozen of what can be forty thousand lines, so the
/// scrollbar answers "how far down am I" and nothing else. This answers the
/// question a log actually gets asked — *where are the bad parts, and am I near
/// one* — by marking every error and warning at its position in the whole file,
/// with a bracket showing what is currently on screen.
fn render_log_minimap(
    f: &mut Frame,
    area: Rect,
    state: &AppState,
    first: usize,
    viewport: usize,
) {
    let theme = &state.theme;
    let total = state.log_rendered.len();
    let rows = area.height.saturating_sub(2) as usize;
    if rows == 0 {
        return;
    }
    // Which rendered lines fall in each map row, and whether any is bad.
    let mut marks = vec![None; rows];
    let bucket = |line: usize| (line * rows) / total.max(1);
    let mut mark = |line: usize, style: u8| {
        if let Some(slot) = marks.get_mut(bucket(line)) {
            // An error in the same band as a warning wins: it is the one you are
            // looking for, and a band can only say one thing.
            *slot = Some(slot.map_or(style, |s: u8| s.max(style)));
        }
    };
    // `log_*_lines` index the raw log; the map is over rendered rows, which
    // folding can make fewer. `log_rendered_src` maps one to the other.
    let rendered_row = |raw: usize| -> Option<usize> {
        state.log_rendered_src.iter().position(|s| *s == raw)
    };
    for l in &state.log_warn_lines {
        if let Some(r) = rendered_row(*l) {
            mark(r, 1);
        }
    }
    for l in &state.log_error_lines {
        if let Some(r) = rendered_row(*l) {
            mark(r, 2);
        }
    }

    let (top, bottom) = (bucket(first), bucket((first + viewport).min(total)));
    let x = area.x + area.width - 2;
    for (i, m) in marks.iter().enumerate() {
        let here = i >= top && i <= bottom;
        let (ch, color) = match m {
            Some(2) => ("█", theme.failure),
            Some(1) => ("▓", theme.warning),
            // The unmarked track still shows where you are.
            _ if here => ("│", theme.text_faint),
            _ => ("·", theme.text_ghost),
        };
        let style = if here {
            Style::default().fg(color).bg(theme.surface_alt)
        } else {
            Style::default().fg(color)
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(ch, style))),
            Rect { x, y: area.y + 1 + i as u16, width: 1, height: 1 },
        );
    }
}

fn render_search_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
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
        Span::styled(buf.as_str(), Style::default().fg(theme.text_bright)),
        Span::styled("█", Style::default().fg(accent)),
    ]);

    f.render_widget(Clear, popup_area);
    f.render_widget(block, popup_area);
    f.render_widget(Paragraph::new(content), inner);
}

fn render_watch(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let summary_lines = if let Some(detail) = &state.run_detail {
        let elapsed = format_elapsed(elapsed_seconds(&detail.run));
        let step = detail.current_step().unwrap_or("—");
        vec![
            Line::from(vec![
                Span::styled("Status: ", Style::default().fg(theme.primary).bold()),
                Span::styled(
                    format!("{:?}", detail.run.status),
                    style_for_status(detail.run.status, &state.theme),
                ),
            ]),
            Line::from(vec![
                Span::styled("Step:   ", Style::default().fg(theme.primary).bold()),
                Span::raw(step.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Elapsed:", Style::default().fg(theme.primary).bold()),
                Span::raw(format!(" {}", elapsed)),
            ]),
            Line::from(vec![
                Span::styled("Run:    ", Style::default().fg(theme.primary).bold()),
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
                Span::styled(job.name.clone(), Style::default().fg(theme.text_bright).bold()),
                Span::styled(
                    format!("  {}/{}", done as u32, total as u32),
                    Style::default().fg(theme.text_muted),
                ),
            ]);
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .label(label)
                    .filled_style(g_style)
                    .unfilled_style(Style::default().fg(theme.border)),
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
                        Style::default().fg(theme.success_dim),
                    ),
                    Status::Failure =>  (
                        Style::default().fg(theme.failure).bold(),
                        Style::default().fg(theme.failure_dim).bold(),
                    ),
                    Status::Running =>  (
                        style_for_status(step.status, theme).bold(),
                        Style::default().fg(theme.text_bright).bold(),
                    ),
                    Status::Cancelled | Status::Skipped => (
                        Style::default().fg(theme.unknown),
                        Style::default().fg(theme.text_ghost),
                    ),
                    _ => (
                        Style::default().fg(theme.text_ghost),
                        Style::default().fg(theme.text_ghost),
                    ),
                };
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(animated_glyph(step.status, state.tick_count), glyph_style),
                    Span::styled(format!(" {}. ", si + 1), Style::default().fg(theme.text_ghost)),
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
    let theme = &state.theme;
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
            Style::default().fg(theme.text_bright).bold(),
        ),
        Span::raw("   vs   "),
        match &baseline {
            Some(b) => Span::styled(
                format!("last success #{}", b.run_id),
                Style::default().fg(theme.success).bold(),
            ),
            None => Span::styled(
                "no successful run in history",
                Style::default().fg(theme.text_faint).italic(),
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
                Span::styled(step.name.clone(), Style::default().fg(theme.text_bright)),
                Span::raw("  "),
                Span::styled(prev_text, Style::default().fg(theme.text_faint)),
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
                Style::default().fg(theme.text_faint).italic(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "browse a few completed runs first to populate history",
                Style::default().fg(theme.text_faint).italic(),
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
        Status::Queued => Style::default().fg(theme.info).bold(),
        Status::Cancelled => Style::default().fg(theme.unknown),
        Status::Skipped => Style::default().fg(theme.unknown),
        Status::Unknown => Style::default().fg(theme.unknown),
    }
}

fn row_bg_for_status(s: Status, theme: &Theme) -> Color {
    match s {
        Status::Failure            => theme.row_failure,
        Status::Running            => theme.row_running,
        Status::Queued             => theme.row_queued,
        Status::Cancelled
        | Status::Skipped          => theme.row_idle,
        _                          => theme.surface,
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
    let theme = &state.theme;
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
            Style::default().bold().fg(theme.primary)
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
            Style::default().fg(theme.accent).bold()
        } else if selected {
            Style::default().fg(theme.text_bright).bold()
        } else {
            Style::default().fg(theme.text_muted)
        };
        let mut spans = vec![
            Span::raw(prefix),
            Span::styled(format!("{:<width$}", field.name, width = name_width), label_style),
            Span::raw("  "),
            Span::styled(value_display, value_style),
            Span::styled(editing_marker, Style::default().fg(theme.accent)),
        ];
        if field.recalled {
            // A prefilled value someone did not notice is how a deploy goes to
            // yesterday's target — the recall earns its keystroke saved only
            // if it is visibly a recall.
            spans.push(Span::styled(
                "  ↺ last used",
                Style::default().fg(theme.text_faint),
            ));
        }
        if field.required {
            spans.push(Span::styled(
                "  (required)",
                Style::default().fg(theme.failure),
            ));
        }
        if let Some(opts) = &field.options {
            spans.push(Span::styled(
                format!("  [{}]", opts.join("/")),
                Style::default().fg(theme.text_faint),
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
        Span::styled(submit_key, Style::default().fg(theme.text_bright).bold()),
        Span::styled(" trigger  ", Style::default().fg(theme.text_faint)),
        Span::styled(cancel_key, Style::default().fg(theme.text_bright).bold()),
        Span::styled(" cancel", Style::default().fg(theme.text_faint)),
    ];
    if has_options {
        hint_spans.push(Span::styled("  Space", Style::default().fg(theme.text_bright).bold()));
        hint_spans.push(Span::styled(" cycle", Style::default().fg(theme.text_faint)));
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

fn relative_styled(t: chrono::DateTime<Utc>, theme: &Theme) -> (String, Style) {
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
        Style::default().fg(theme.text_bright)
    } else if secs < 86400 {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.text_faint)
    };
    (text, style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::github::ApiError;
    use chrono::TimeZone;

    /// Draw the Logs pane into an off-screen terminal and return it as text,
    /// so the assertions below are about what a user actually sees rather than
    /// about the state that feeds it.
    fn draw_logs(state: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_logs(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn noisy_log_state(focus: bool) -> AppState {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let mut lines: Vec<String> = (0..400).map(|i| format!("compiling crate {i}")).collect();
        lines.push("##[error]test tui::foo failed".into());
        lines.extend((0..30).map(|i| format!("trailing {i}")));
        st.log_lines = lines;
        st.log_focus_context = 1;
        st.init_log_groups();
        st.log_focus = focus;
        st.last_logs_viewport_height.set(10);
        st.last_logs_viewport_width.set(80);
        st.recompute_log_rendered();
        st
    }

    #[test]
    #[ignore = "visual check: cargo test show_logs -- --ignored --nocapture"]
    fn show_logs() {
        println!("─── unfocused ───\n{}", draw_logs(&noisy_log_state(false), 80, 12));
        println!("─── focus (F) ───\n{}", draw_logs(&noisy_log_state(true), 80, 12));
    }

    #[test]
    fn unfocused_title_advertises_the_error_count() {
        let out = draw_logs(&noisy_log_state(false), 80, 12);
        let title = out.lines().next().unwrap().to_string();
        // Whether the log contains anything wrong is the first thing to answer,
        // and scrolling 431 lines is the slow way to ask.
        assert!(title.contains("431 lines"), "got {title:?}");
        assert!(title.contains("1✗"), "got {title:?}");
    }

    /// The minimap column of a drawn Logs pane, top to bottom.
    fn map_column(out: &str) -> String {
        out.lines()
            .filter(|l| l.starts_with('│'))
            .filter_map(|l| l.chars().rev().nth(1))
            .collect()
    }

    #[test]
    fn the_log_map_shows_where_the_errors_are_from_anywhere_in_the_file() {
        let out = draw_logs(&noisy_log_state(false), 80, 12);
        let col = map_column(&out);

        // The error is 400 lines below the viewport. Its mark is on screen from
        // the very top of the log — that is the whole point of the column.
        assert!(col.contains('█'), "got {col:?}");
        assert!(col.ends_with('█'), "near the end of the log, got {col:?}");
        // …and the bracket says where you are, which is the top.
        assert!(col.starts_with('│'), "got {col:?}");
        assert_eq!(col.matches('│').count(), 1, "the viewport is one band, got {col:?}");
    }

    #[test]
    fn a_log_that_fits_on_screen_gets_no_map() {
        let mut st = noisy_log_state(false);
        st.log_lines.truncate(6);
        st.init_log_groups();
        st.recompute_log_rendered();
        let out = draw_logs(&st, 80, 12);

        // Mapping six lines onto ten rows says nothing, and the column it costs
        // is a column of log.
        assert_eq!(map_column(&out).trim(), "", "got:\n{out}");
    }

    #[test]
    fn focus_mode_draws_the_error_between_two_folds() {
        let out = draw_logs(&noisy_log_state(true), 80, 12);
        assert!(out.contains("399 lines hidden"), "got:\n{out}");
        assert!(out.contains("test tui::foo failed"), "got:\n{out}");
        assert!(out.contains("29 lines hidden"), "got:\n{out}");
        // 431 lines reduced to a screen that fits: 2 folds + 3 kept lines.
        let body: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with('│') && !l.trim_matches(['│', ' ']).is_empty())
            .collect();
        assert_eq!(body.len(), 5, "got {body:?}");
    }

    /// Draw the Changes pane off-screen, same idea as `draw_logs`.
    fn draw_changes(state: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_git_status(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn state_with_op(op: GitOp) -> AppState {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let gv = crate::app::state::GitView::new(
            "acme/api".into(),
            std::path::PathBuf::from("/tmp/acme-api"),
            true,
        );
        st.git_ops.insert(gv.spec.clone(), op);
        st.git_view = Some(gv);
        st.view = View::GitStatus;
        st
    }

    /// A budget `pct` spent, refilling at `reset`.
    fn quota_at(pct: u32, reset: chrono::DateTime<Utc>) -> crate::provider::github::Quota {
        crate::provider::github::Quota { limit: 100, used: pct, reset }
    }

    /// The push question over an open working tree, as it is drawn.
    fn push_prompt_screen(has_upstream: bool) -> (String, ratatui::buffer::Buffer) {
        let mut st = state_with_op(GitOp::new("commit", None, 0));
        st.git_ops.clear();
        st.push_prompt = Some(crate::app::state::PushPrompt {
            spec: "acme/api".into(),
            branch: "main".into(),
            has_upstream,
            yes: true,
            batch_count: None,
        });
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 14)).unwrap();
        term.draw(|f| render(f, &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let text = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buf)
    }

    /// Background of the first cell of `word`, wherever it is on screen.
    fn bg_of(buf: &ratatui::buffer::Buffer, word: &str) -> Color {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if let Some(i) = row.find(word) {
                return buf[(i as u16, y)].bg;
            }
        }
        panic!("{word} is not on screen");
    }

    #[test]
    fn a_landed_commit_puts_the_push_question_in_the_middle_of_the_screen() {
        let (text, buf) = push_prompt_screen(true);
        assert!(text.contains("Push?"), "{text}");
        assert!(text.contains("push main to origin?"), "{text}");
        // Yes is the answer wearing the highlight, so Enter is visibly safe —
        // a default you have to read the hint line to learn is not a default.
        let theme = Theme::default();
        assert_eq!(bg_of(&buf, "Yes"), theme.accent);
        assert_ne!(bg_of(&buf, "No"), theme.accent);
        // The keys on offer are the box's, not the working tree's underneath.
        assert!(text.contains("Esc") && !text.contains("stage"), "{text}");
    }

    #[test]
    fn a_first_push_says_it_is_creating_the_branch() {
        // "push main" and "publish main" are different acts, and only one of
        // them puts a new branch on the remote for everyone else to see.
        let (text, _) = push_prompt_screen(false);
        assert!(text.contains("publish main to origin?"), "{text}");
    }

    #[test]
    fn a_failed_hook_shows_the_failure_not_just_that_it_failed() {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in [
            "ruff.....Passed",
            "pytest...Failed",
            "FAILED tests/test_api.py::test_login",
            "assert 1 == 2",
        ] {
            op.push_line(l.into(), false);
        }
        op.finished = true;
        op.failed = true;
        let out = draw_changes(&state_with_op(op), 80, 14);

        // The whole point: the pytest line is on screen, and the border names
        // what produced it rather than saying "commit failed".
        assert!(out.contains("pre-commit hook failed"), "got:\n{out}");
        assert!(out.contains("test_login"), "got:\n{out}");
        assert!(out.contains("assert 1 == 2"), "got:\n{out}");
        // Both `pytest...Failed` and the `FAILED` line count as errors.
        assert!(out.contains("2✗"), "got:\n{out}");
    }

    #[test]
    fn a_running_hook_says_what_it_is_and_how_long_it_has_been() {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        for i in 0..40 {
            op.push_line(format!("collecting test {i}"), false);
        }
        let mut st = state_with_op(op);
        st.tick_count = 123; // 12.3s in
        let out = draw_changes(&st, 80, 14);

        assert!(out.contains("pre-commit hook · 12s"), "got:\n{out}");
        // Still running, so the pane sits on the newest output — the oldest is
        // scrolled off, not the other way round.
        assert!(out.contains("collecting test 39"), "got:\n{out}");
        assert!(!out.contains("collecting test 0 "), "got:\n{out}");
    }

    #[test]
    fn a_clean_tree_ahead_of_upstream_says_so_instead_of_nothing_to_commit() {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let mut gv = crate::app::state::GitView::new(
            "acme/api".into(),
            std::path::PathBuf::from("/tmp/acme-api"),
            true,
        );
        // The state right after a commit: nothing left to stage, but the
        // commit only exists locally.
        gv.status = Some(crate::git::parse_status(
            "## main...origin/main [ahead 1]\0",
        ));
        st.git_view = Some(gv);
        st.view = View::GitStatus;
        let out = draw_changes(&st, 80, 14);

        // "Nothing to commit" reads as "done" — but the work isn't visible to
        // anyone until it is pushed, and this line has to say that.
        assert!(out.contains("↑1 commit not pushed"), "got:\n{out}");
        assert!(out.contains("P pushes"), "got:\n{out}");
        assert!(!out.contains("nothing to commit"), "got:\n{out}");

        // Once upstream has it, clean means clean.
        st.git_view.as_mut().unwrap().status =
            Some(crate::git::parse_status("## main...origin/main\0"));
        let out = draw_changes(&st, 80, 14);
        assert!(out.contains("nothing to commit"), "got:\n{out}");
    }

    #[test]
    #[ignore = "visual check: cargo test show_changes -- --ignored --nocapture"]
    fn show_changes() {
        let mut running = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in ["ruff.....Passed", "pyright..", "collecting tests…"] {
            running.push_line(l.into(), false);
        }
        let mut st = state_with_op(running);
        st.tick_count = 87;
        println!("─── running ───\n{}", draw_changes(&st, 80, 14));

        let mut failed = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in [
            "ruff.....Passed",
            "pyright..Failed",
            "- hook id: pyright",
            "src/api.py:41:12 - error: \"user_id\" is not defined",
            "1 error, 0 warnings",
        ] {
            failed.push_line(l.into(), false);
        }
        failed.finished = true;
        failed.failed = true;
        println!("─── failed ───\n{}", draw_changes(&state_with_op(failed), 80, 14));
    }

    #[test]
    fn the_dashboard_row_says_a_repo_is_mid_commit() {
        let mut st = state_with_op(GitOp::new("commit", Some("pre-commit".into()), 0));
        // The row for the repo being committed, plus one that isn't.
        for spec in ["acme/api", "acme/web"] {
            let mut card = crate::app::state::RepoCard::new(spec.into());
            card.path = Some(std::path::PathBuf::from("/tmp").join(spec));
            card.git = Some(crate::git::parse_status("## main\0 M a.txt\0"));
            card.loaded = true;
            st.repos.push(card);
        }
        st.view = View::Repos;

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 10)).unwrap();
        term.draw(|f| render_repos(f, f.area(), &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        let api = rows.iter().find(|r| r.contains("acme/api")).unwrap();
        let web = rows.iter().find(|r| r.contains("acme/web")).unwrap();
        // The in-flight commit rides with the branch; the file count now has a
        // column of its own.
        assert!(api.contains("main"), "got {api:?}");
        assert!(api.contains("commit"), "got {api:?}");
        assert!(api.contains("●1"), "got {api:?}");
        // …and only on that repo. A marker on every row would say nothing.
        assert!(!web.contains("commit"), "got {web:?}");
    }

    fn a_run(id: u64, title: &str, status: Status, age_secs: i64) -> Run {
        let now = Utc::now();
        Run {
            id,
            display_title: title.into(),
            head_branch: "main".into(),
            commit_msg: "wip".into(),
            status,
            created_at: now - chrono::Duration::seconds(age_secs),
            updated_at: now,
            url: String::new(),
            workflow_file: Some("deploy.yml".into()),
        }
    }

    fn a_job(name: &str, steps: &[(&str, Status)]) -> crate::provider::Job {
        crate::provider::Job {
            id: 7,
            name: name.into(),
            status: Status::Running,
            steps: steps
                .iter()
                .map(|(n, s)| crate::provider::Step {
                    name: (*n).into(),
                    status: *s,
                    started_at: None,
                    completed_at: None,
                })
                .collect(),
        }
    }

    /// Three repos, two of them mid-deploy — one on a step, one still queued.
    fn dashboard_with_live_ci() -> AppState {
        let mut st = AppState::new(
            "muufree/backend".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.view = View::Repos;
        for spec in ["muufree/backend", "muufree/cms", "muufree/website"] {
            let mut card = crate::app::state::RepoCard::new(spec.into());
            card.git = Some(crate::git::parse_status("## main\0"));
            card.loaded = true;
            st.repos.push(card);
        }
        st.repos[0].runs = vec![a_run(1, "Deploy to Stage", Status::Running, 102)];
        st.repos[1].runs = vec![a_run(2, "Deploy to Stage", Status::Queued, 9)];
        st.repos[2].runs = vec![a_run(3, "CodeQL Security Risk", Status::Success, 400)];

        st.run_progress.insert(
            "muufree/backend".into(),
            vec![crate::provider::RunDetail {
                run: st.repos[0].runs[0].clone(),
                jobs: vec![a_job(
                    "build",
                    &[
                        ("Checkout", Status::Success),
                        ("Set up Docker", Status::Success),
                        ("Run migrations", Status::Running),
                        ("Push image", Status::Queued),
                    ],
                )],
            }],
        );
        st.run_progress.insert(
            "muufree/cms".into(),
            vec![crate::provider::RunDetail {
                run: st.repos[1].runs[0].clone(),
                jobs: Vec::new(),
            }],
        );
        st
    }

    fn draw_repos(state: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_repos(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_strip_says_which_step_a_running_deploy_is_on() {
        let out = draw_repos(&dashboard_with_live_ci(), 150, 16);
        // The row above can only say "running". The question you actually have
        // while waiting on a deploy is *what* it is doing, and this is it.
        assert!(out.contains("Run migrations"), "got:\n{out}");
        assert!(out.contains("build"), "got:\n{out}");
        // Two done of four steps, sitting on the third.
        assert!(out.contains("3/4"), "got:\n{out}");
        // 1:42 in — a deploy that has been going too long has to be visible.
        assert!(out.contains("1:42"), "got:\n{out}");
        assert!(out.contains("2 in flight"), "got:\n{out}");
    }

    #[test]
    fn a_repo_running_two_workflows_gets_a_line_for_each() {
        // One push commonly starts CI and a deploy at once. A strip that names
        // only the first says "1 in flight" while the repo's Actions tab says
        // two, and the run you are actually waiting on can be the hidden one.
        let mut st = dashboard_with_live_ci();
        let second = a_run(11, "CI for Backend", Status::Running, 61);
        st.repos[0].runs.push(second.clone());
        st.run_progress.get_mut("muufree/backend").unwrap().push(
            crate::provider::RunDetail {
                run: second,
                jobs: vec![a_job("test (3.13)", &[("Run tests with pytest", Status::Running)])],
            },
        );

        let out = draw_repos(&st, 150, 16);
        assert!(out.contains("3 in flight"), "got:\n{out}");
        assert!(out.contains("Deploy to Stage"), "got:\n{out}");
        assert!(out.contains("CI for Backend"), "got:\n{out}");
        // Both lines carry the repo they belong to; the second is just quieter.
        let strip: String = out
            .lines()
            .skip_while(|l| !l.contains("in flight"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(strip.matches("muufree/backend").count(), 2, "got:\n{out}");
    }

    #[test]
    fn the_strip_lines_its_fields_up_whatever_length_the_names_are() {
        // The failure this is here for: workflow and job names differ in length
        // by a dozen characters between rows, and when two fields shared a cell
        // every row put its branch, its job and its step at a different x. The
        // strip is read down its columns — ragged ones make it unreadable, and
        // the longest row's branch fell off the end of the cell entirely.
        let mut st = dashboard_with_live_ci();
        st.repos[1].runs = vec![a_run(2, "Build & Test Frontend Everywhere", Status::Running, 40)];
        st.run_progress.insert(
            "muufree/cms".into(),
            vec![crate::provider::RunDetail {
                run: st.repos[1].runs[0].clone(),
                jobs: vec![a_job(
                    "build-and-push-the-frontend",
                    &[("Build frontend container", Status::Running)],
                )],
            }],
        );
        st.repos[2].runs = vec![a_run(3, "CI", Status::Running, 5)];
        st.run_progress.insert(
            "muufree/website".into(),
            vec![crate::provider::RunDetail {
                run: st.repos[2].runs[0].clone(),
                jobs: vec![a_job("t", &[("Lint", Status::Running)])],
            }],
        );

        let out = draw_repos(&st, 150, 16);
        let strip: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.contains("in flight"))
            .filter(|l| l.contains("main"))
            .collect();
        assert_eq!(strip.len(), 3, "got:\n{out}");
        // Which *column* the field starts in — `find` answers in bytes, and an
        // ellipsis on one row would move the answer without moving the pixel.
        let at = |needle: &str| -> Vec<usize> {
            strip
                .iter()
                .map(|l| l.find(needle).map(|b| disp_width(&l[..b])).unwrap_or(usize::MAX))
                .collect()
        };
        let branch = at("main");
        assert!(
            branch.iter().all(|c| *c == branch[0]),
            "branches start at {branch:?}, got:\n{out}"
        );
        let step = at("›");
        assert!(
            step.iter().all(|c| *c == step[0]),
            "steps start at {step:?}, got:\n{out}"
        );
    }

    #[test]
    fn a_cut_field_is_measured_in_columns_not_characters() {
        // Workflow names carry emoji, and every one is two columns wide. Cut by
        // `char` count they overran the cell they were cut to fit, and ratatui
        // clipped whatever came next — which is how a branch went missing from
        // one row of the strip while the row above it was fine.
        // Never over the cell — a two-column glyph that doesn't fit is dropped
        // whole rather than half-drawn into the next field.
        assert!(disp_width(&truncate("🔨🔨🔨🔨 CI", 6)) <= 6);
        assert!(disp_width(&truncate("🔨🔨🔨🔨 CI", 7)) <= 7);
        assert_eq!(disp_width(&truncate("Build & Test Frontend", 10)), 10);
        // Room for all of it is room for all of it — no ellipsis for its own sake.
        assert_eq!(truncate("🔨 CI", 8), "🔨 CI");
        assert_eq!(truncate("", 4), "");
    }

    #[test]
    fn a_queued_run_says_it_is_waiting_rather_than_inventing_a_step() {
        let out = draw_repos(&dashboard_with_live_ci(), 150, 16);
        assert!(out.contains("waiting for a runner"), "got:\n{out}");
        // The settled repo has no business in a strip about what's in flight.
        let strip: String = out
            .lines()
            .skip_while(|l| !l.contains("in flight"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!strip.contains("muufree/website"), "got:\n{strip}");
    }

    fn batch_state(phase: BatchPhase) -> AppState {
        let mut st = dashboard_with_live_ci();
        st.view = View::BatchCommit;
        let items = ["muufree/backend", "muufree/cms", "muufree/flutter"]
            .iter()
            .map(|s| {
                crate::app::state::BatchItem::new(
                    (*s).into(),
                    std::path::PathBuf::from("/tmp").join(s),
                )
            })
            .collect();
        let mut b = crate::app::state::BatchCommit::new(items, 0);
        b.message = "chore: bump shared client to 2.1".into();
        b.input = (phase == BatchPhase::Compose).then(|| "chore: bump shared".to_string());
        b.phase = phase;
        if phase != BatchPhase::Compose {
            b.items[0].state = crate::app::state::ItemState::Committed;
            b.items[0].sha = Some("9f2c1ab".into());
            b.cursor = 1;
            b.items[1].state = match phase {
                BatchPhase::Paused => {
                    crate::app::state::ItemState::Failed("pytest failed".into())
                }
                _ => crate::app::state::ItemState::Running,
            };
            let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
            for l in ["ruff.....Passed", "pytest...Failed", "assert 1 == 2"] {
                op.push_line(l.into(), false);
            }
            op.finished = phase == BatchPhase::Paused;
            op.failed = phase == BatchPhase::Paused;
            st.git_ops.insert("muufree/cms".into(), op);
        }
        st.batch = Some(b);
        st
    }

    fn draw_batch(state: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_batch_commit(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_message_box_says_how_many_repos_one_message_is_about_to_hit() {
        let out = draw_batch(&batch_state(BatchPhase::Compose), 110, 14);
        // The whole risk of the feature is committing to more repos than you
        // meant to, so the count and the list are on screen before Enter.
        assert!(out.contains("One message for 3 repos"), "got:\n{out}");
        assert!(out.contains("chore: bump shared"), "got:\n{out}");
        for spec in ["muufree/backend", "muufree/cms", "muufree/flutter"] {
            assert!(out.contains(spec), "{spec} missing from:\n{out}");
        }
        assert!(out.contains("git add -A"), "got:\n{out}");
    }

    #[test]
    fn one_marked_repo_is_a_repo_not_a_repos() {
        let mut st = batch_state(BatchPhase::Compose);
        if let Some(b) = st.batch.as_mut() {
            b.items.truncate(1);
        }
        let out = draw_batch(&st, 110, 14);
        assert!(out.contains("Commit 1 repo "), "got:\n{out}");
        assert!(out.contains("One message for 1 repo "), "got:\n{out}");

        // The caret gets a line to itself, with air above and below it.
        let caret = out.lines().position(|l| l.contains('█')).unwrap();
        let above = out.lines().nth(caret - 1).unwrap();
        let below = out.lines().nth(caret + 1).unwrap();
        assert_eq!(above.trim_matches(['│', ' ']), "", "got {above:?}");
        assert_eq!(below.trim_matches(['│', ' ']), "", "got {below:?}");
    }

    #[test]
    fn a_running_batch_says_which_repo_it_is_on_and_what_the_hook_is_saying() {
        let out = draw_batch(&batch_state(BatchPhase::Committing), 110, 16);
        assert!(out.contains("Committing · 2/3"), "got:\n{out}");
        // What is done, what is happening, what is waiting — all three.
        assert!(out.contains("committed 9f2c1ab"), "got:\n{out}");
        assert!(out.contains("committing…"), "got:\n{out}");
        assert!(out.contains("queued"), "got:\n{out}");
        // The hook's own output, live, is the reason this view exists.
        assert!(out.contains("ruff.....Passed"), "got:\n{out}");
    }

    #[test]
    fn a_paused_batch_shows_the_failure_and_what_can_be_done_about_it() {
        let out = draw_batch(&batch_state(BatchPhase::Paused), 110, 16);
        assert!(out.contains("Paused on muufree/cms"), "got:\n{out}");
        assert!(out.contains("r retry"), "got:\n{out}");
        assert!(out.contains("s skip"), "got:\n{out}");
        // Not just "it failed": the assertion that failed is on screen.
        assert!(out.contains("assert 1 == 2"), "got:\n{out}");
        // The repo behind it kept its commit, and says so.
        assert!(out.contains("committed 9f2c1ab"), "got:\n{out}");
    }

    #[test]
    fn pushing_is_offered_never_assumed() {
        let mut st = batch_state(BatchPhase::AskPush);
        if let Some(b) = st.batch.as_mut() {
            b.items[1].state = crate::app::state::ItemState::Committed;
            b.items[1].sha = Some("77aa310".into());
            b.items[2].state = crate::app::state::ItemState::Nothing("working tree is clean".into());
        }
        st.git_ops.clear();
        let out = draw_batch(&st, 110, 12);
        assert!(out.contains("2 committed"), "got:\n{out}");
        assert!(out.contains("pushes them all"), "got:\n{out}");
        // A clean repo is reported, not quietly dropped from the list.
        assert!(out.contains("working tree is clean"), "got:\n{out}");
    }

    #[test]
    fn the_dashboard_shows_what_a_batch_would_take() {
        let mut st = dashboard_with_live_ci();
        assert!(
            !draw_footer(&st, 150).contains("marked"),
            "unmarked dashboard says nothing"
        );

        st.repo_marks.insert("muufree/backend".into());
        st.repo_marks.insert("muufree/cms".into());
        // Beside the key that would act on them, now that the panel title they
        // used to sit in is gone.
        let bar = draw_footer(&st, 150);
        assert!(bar.contains("◆ 2 marked"), "got:\n{bar}");
        assert!(bar.contains("commit marked"), "got:\n{bar}");
        let out = draw_repos(&st, 150, 16);
        // …and which rows they are, not just how many.
        let backend = out.lines().find(|l| l.contains("muufree/backend")).unwrap();
        let website = out.lines().find(|l| l.contains("muufree/website")).unwrap();
        assert!(backend.contains('◆'), "got {backend:?}");
        assert!(!website.contains('◆'), "got {website:?}");
    }

    #[test]
    fn a_queue_taller_than_the_screen_follows_the_repo_being_worked_on() {
        let mut st = batch_state(BatchPhase::Committing);
        if let Some(b) = st.batch.as_mut() {
            b.items = (0..12)
                .map(|i| {
                    crate::app::state::BatchItem::new(
                        format!("acme/repo{i:02}"),
                        std::path::PathBuf::from("/tmp"),
                    )
                })
                .collect();
            b.cursor = 9;
            b.items[9].state = crate::app::state::ItemState::Running;
            for item in b.items.iter_mut().take(9) {
                item.state = crate::app::state::ItemState::Committed;
                item.sha = Some("9f2c1ab".into());
            }
        }
        st.git_ops.clear();
        let out = draw_batch(&st, 110, 10);

        // The repo in flight is the one you are waiting on; it must be visible.
        assert!(out.contains("acme/repo09"), "got:\n{out}");
        // And the rows that didn't fit are counted, not silently dropped.
        assert!(out.contains("more · 9 committed"), "got:\n{out}");
    }

    #[test]
    #[ignore = "visual check: cargo test show_batch_commit -- --ignored --nocapture"]
    fn show_batch_commit() {
        for (label, phase) in [
            ("compose", BatchPhase::Compose),
            ("committing", BatchPhase::Committing),
            ("paused", BatchPhase::Paused),
            ("done", BatchPhase::Done),
        ] {
            println!("─── {label} ───\n{}", draw_batch(&batch_state(phase), 110, 14));
        }
    }

    /// The background colour of the row naming `spec`.
    fn row_bg(state: &AppState, spec: &str) -> Color {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 12)).unwrap();
        term.draw(|f| render_repos(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        let y = (0..buf.area.height)
            .find(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(spec)
            })
            .expect("row on screen");
        buf[(20, y)].bg
    }

    /// Every cell of the row a repo is on, as (symbol, fg, bg).
    fn row_cells(state: &AppState, spec: &str) -> Vec<(String, Color, Color)> {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 12)).unwrap();
        term.draw(|f| render_repos(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        let y = (0..buf.area.height)
            .find(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(spec)
            })
            .expect("row on screen");
        (0..buf.area.width)
            .map(|x| {
                let c = &buf[(x, y)];
                (c.symbol().to_string(), c.fg, c.bg)
            })
            .collect()
    }

    #[test]
    fn the_selected_row_still_shows_what_it_is_selected_to_show() {
        let mut st = dashboard_with_live_ci();
        st.repos[2].runs = vec![a_run(9, "Deploy", Status::Failure, 60)];
        let theme = Theme::default();
        let has_failure_fg = |st: &AppState| {
            row_cells(st, "muufree/website")
                .iter()
                .any(|(_, fg, _)| *fg == theme.failure)
        };
        // Unselected, the row shows a red ✗ and a red "Deploy".
        st.repo_cursor = 0;
        assert!(has_failure_fg(&st));

        // Selected, it must still. The old highlight painted one flat style over
        // the whole row, so the single repo you were pointing at was the one
        // whose status you could not read.
        st.repo_cursor = 2;
        assert!(
            has_failure_fg(&st),
            "the selection erased the status it was pointing at"
        );
        // And it is still visibly selected — the tint lands in the background.
        let bgs: Vec<_> = row_cells(&st, "muufree/website")
            .iter()
            .map(|(_, _, bg)| *bg)
            .collect();
        st.repo_cursor = 0;
        let plain: Vec<_> = row_cells(&st, "muufree/website")
            .iter()
            .map(|(_, _, bg)| *bg)
            .collect();
        assert_ne!(bgs, plain);
    }

    #[test]
    fn every_other_row_sits_a_hair_off_the_one_above_it() {
        let theme = Theme::default();
        // Sensed, not seen: the band exists so the eye can hold one line across
        // eight columns, and a stripe you notice is a stripe that has failed.
        assert_eq!(banded(theme.row_idle, 0, &theme), theme.row_idle);
        let odd = banded(theme.row_idle, 1, &theme);
        assert_ne!(odd, theme.row_idle);
        let (Color::Rgb(r, g, b), Color::Rgb(br, bg_, bb)) = (odd, theme.row_idle) else {
            unreachable!("the default theme is true colour")
        };
        let step = (r as i32 - br as i32) + (g as i32 - bg_ as i32) + (b as i32 - bb as i32);
        assert!((1..=24).contains(&step), "a band of {step} is a stripe");
    }

    #[test]
    fn the_updated_column_lines_its_units_up() {
        let mut st = dashboard_with_live_ci();
        st.run_progress.clear();
        // Widths that would be ragged if the column were left-aligned.
        st.repos[0].runs = vec![a_run(1, "CI", Status::Success, 60)];
        st.repos[0].runs[0].updated_at = Utc::now() - chrono::Duration::days(130);
        st.repos[1].runs = vec![a_run(2, "CI", Status::Success, 60)];
        st.repos[1].runs[0].updated_at = Utc::now() - chrono::Duration::days(4);

        let end = |spec: &str| {
            let cells = row_cells(&st, spec);
            let text: String = cells.iter().map(|(s, _, _)| s.as_str()).collect();
            let at = text.find(" ago").expect("an Updated cell") + " ago".len();
            text[..at].chars().count()
        };
        assert_eq!(end("muufree/backend"), end("muufree/cms"));
    }

    #[test]
    fn owners_get_one_heading_each_and_the_cursor_still_lands_on_repos() {
        let mut st = dashboard_with_live_ci();
        let mut gam = crate::app::state::RepoCard::new("drposture/gam".into());
        gam.loaded = true;
        st.repos.push(gam);

        // Tall enough that the live strip does not clip the last group off.
        let out = draw_repos(&st, 150, 22);
        assert_eq!(out.matches("muufree ╌").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("drposture ╌").count(), 1, "got:\n{out}");
        // The owner is said once, in the heading, not repeated down the column.
        // (The live strip below still names repos in full — it has no heading
        // to inherit the owner from.)
        let row = out
            .lines()
            .find(|l| l.contains("backend") && l.contains("clean"))
            .unwrap();
        assert!(row.contains(" backend"), "got {row:?}");
        assert!(!row.contains("muufree/"), "got {row:?}");

        // Headings push every row below them down, so the highlight has to be
        // translated — otherwise the cursor drifts onto a heading.
        for (cursor, expect) in [(0usize, "backend"), (3, "gam")] {
            st.repo_cursor = cursor;
            let out = draw_repos(&st, 150, 22);
            let marked = out.lines().find(|l| l.contains('▶')).unwrap();
            assert!(marked.contains(expect), "cursor {cursor}: got {marked:?}");
        }
    }

    #[test]
    fn one_owner_is_not_worth_a_heading() {
        let st = dashboard_with_live_ci();
        let out = draw_repos(&st, 150, 16);
        assert!(!out.contains("╌╌╌"), "got:\n{out}");
        // …and with nothing to group by, the full spec stays on the row.
        assert!(out.contains("muufree/backend"), "got:\n{out}");
    }

    #[test]
    fn columns_go_whole_rather_than_all_shrinking_together() {
        // What survives narrowing is what the view is for: which repo, what its
        // CI did, and whether the tree is dirty.
        let narrow = Columns::for_width(90);
        assert!(!narrow.ran_on && !narrow.recent);
        assert!(Columns::for_width(200).ran_on);
        // History degrades by degrees, so it is the last thing to go entirely.
        assert!(Columns::for_width(110).spark_w < Columns::for_width(200).spark_w);

        let st = dashboard_with_live_ci();
        let out = draw_repos(&st, 90, 12);
        assert!(out.contains("muufree/backend"), "got:\n{out}");
        assert!(out.contains("clean"), "got:\n{out}");
        // The run's branch is one keystroke away in the run list; the repo name
        // truncated to six characters is not recoverable at all.
        assert!(!out.contains("Ran on"), "got:\n{out}");
    }

    #[test]
    fn the_history_bars_say_when_a_repo_started_failing() {
        let theme = Theme::default();
        // Newest first, as the API hands them over: two fresh failures behind a
        // wall of green.
        let runs: Vec<Run> = (0..6)
            .map(|i| {
                let mut r = a_run(i, "CI", if i < 2 { Status::Failure } else { Status::Success }, 60);
                r.updated_at = r.created_at + chrono::Duration::seconds(if i == 5 { 300 } else { 60 });
                r
            })
            .collect();
        let bars = run_sparkline(&runs, 6, &theme);

        // Oldest on the left, so the recent failures are where the eye lands.
        let colors: Vec<Color> = bars.iter().map(|s| s.style.fg.unwrap()).collect();
        // The newest run is drawn at full strength — nothing to fade toward yet.
        assert_eq!(colors[5], theme.failure);
        // Both recent failures still read as failures rather than as background.
        let toward_bg = |c: Color| match (c, theme.row_idle) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg_, bb)) => {
                let d = |x: u8, y: u8| (x as f64 - y as f64).abs();
                d(r, br) + d(g, bg_) + d(b, bb)
            }
            _ => unreachable!("the default theme is true colour"),
        };
        assert!(toward_bg(colors[4]) > toward_bg(colors[0]), "{colors:?}");
        // Older runs recede toward the row's ground, so the strip reads as a
        // timeline rather than as a pattern: every success in the window is
        // dimmer than the one to its right.
        for w in colors[..4].windows(2) {
            assert!(toward_bg(w[0]) < toward_bg(w[1]), "{colors:?}");
        }
        // Height is duration: the five-minute run towers over the one-minute ones.
        let glyphs: Vec<&str> = bars.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(glyphs[0], "█", "the longest run in the window");
        assert!(glyphs[1] < glyphs[0], "got {glyphs:?}");

        // A repo with no runs says so rather than drawing a flat line, which
        // would read as "ran, took no time".
        let none = run_sparkline(&[], 6, &theme);
        let text: String = none.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.trim(), "—");
        // Still the full column wide, so its count sits where every other row
        // keeps theirs.
        assert_eq!(text.chars().count(), 6);

        // A history where every run took about the same time is flat, and must
        // draw flat. Scaling a 3% spread to full height invents a shape, and an
        // invented shape is worse than no chart.
        let steady: Vec<Run> = (0..6)
            .map(|i| {
                let mut r = a_run(i, "CI", Status::Success, 60);
                r.updated_at = r.created_at + chrono::Duration::seconds(60 + i as i64);
                r
            })
            .collect();
        let bars = run_sparkline(&steady, 6, &theme);
        let glyphs: std::collections::HashSet<&str> =
            bars.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(glyphs.len(), 1, "got {glyphs:?}");
    }

    #[test]
    fn a_repo_whose_ci_moved_lights_up_and_settles_back_down() {
        let mut st = dashboard_with_live_ci();
        let settled = row_bg(&st, "muufree/website");

        st.repos[2].changed_tick = Some(100);
        st.tick_count = 100;
        let lit = row_bg(&st, "muufree/website");
        assert_ne!(lit, settled, "a change you did not watch happen is invisible");

        // …and it does not stay lit: a dashboard that keeps glowing at you
        // teaches you to stop looking at the glow.
        st.tick_count = 100 + FLASH_TICKS;
        assert_eq!(row_bg(&st, "muufree/website"), settled);

        // Rows that did not move are untouched throughout.
        st.tick_count = 100;
        assert_eq!(row_bg(&st, "muufree/backend"), {
            st.repos[2].changed_tick = None;
            row_bg(&st, "muufree/backend")
        });
    }

    /// Every row turned away by the same exhausted quota.
    fn dashboard_out_of_quota() -> AppState {
        let mut st = dashboard_with_live_ci();
        st.run_progress.clear();
        for card in st.repos.iter_mut() {
            card.runs.clear();
            card.error = Some(ApiError {
                fault: ApiFault::RateLimited,
                text: ApiFault::RateLimited.label().into(),
            });
        }
        st
    }

    /// The header line of the dashboard, as drawn.
    fn draw_header(state: &AppState, w: u16) -> String {
        draw_one_line(w, |f| render_header(f, f.area(), state))
    }

    /// The footer line, as drawn.
    fn draw_footer(state: &AppState, w: u16) -> String {
        draw_one_line(w, |f| render_footer(f, f.area(), state))
    }

    fn draw_one_line(w: u16, draw: impl FnOnce(&mut Frame)) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 1)).unwrap();
        term.draw(draw).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn the_api_budget_sits_in_the_corner_and_warms_as_it_fills() {
        let theme = Theme::default();
        // Quiet while there is room: this is not what you came to look at.
        assert_eq!(quota_color(&theme, 0), theme.text_faint);
        assert_eq!(quota_color(&theme, 49), theme.text_faint);
        // Then a ramp, arriving at full red exactly where the alarm goes off,
        // so the colour and the sound are saying the same thing.
        assert_eq!(quota_color(&theme, 50), theme.warning);
        assert_eq!(quota_color(&theme, CRITICAL_PERCENT), theme.failure);
        assert_eq!(quota_color(&theme, 100), theme.failure);
        let mid = quota_color(&theme, 70);
        assert_ne!(mid, theme.warning);
        assert_ne!(mid, theme.failure);
    }

    #[test]
    fn the_reset_clock_shows_up_only_once_it_is_the_question() {
        let mut st = dashboard_with_live_ci();
        st.quota = Some(quota_at(40, Utc::now() + chrono::Duration::minutes(23)));
        let calm = draw_header(&st, 120);
        assert!(calm.contains("API 40%"), "{calm}");
        assert!(!calm.contains("till"), "a clock you have no use for is noise: {calm}");

        // Past the line the number stops being trivia and becomes a plan: this
        // is how long to stay out of jog.
        let at = Local::now() + chrono::Duration::minutes(23);
        st.quota = Some(quota_at(94, Utc::now() + chrono::Duration::minutes(23)));
        let hot = draw_header(&st, 120);
        assert!(hot.contains("API 94%"), "{hot}");
        assert!(hot.contains(&format!("till {}", at.format("%H:%M"))), "{hot}");
    }

    #[test]
    fn a_budget_we_have_not_read_yet_shows_nothing_rather_than_zero() {
        let st = dashboard_with_live_ci();
        assert!(st.quota.is_none());
        // "API 0%" would be a lie about a number we do not have, and the most
        // reassuring possible one.
        assert!(!draw_header(&st, 120).contains("API"));
    }

    #[test]
    fn every_repo_wears_the_mark_of_the_forge_it_lives_on() {
        let mut st = dashboard_with_live_ci();
        st.repos[2].remote = None; // a checkout with no GitHub origin
        let out = draw_repos(&st, 150, 12);
        let line = |name: &str| {
            out.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} is not on screen"))
                .to_string()
        };
        assert!(line("backend").contains(crate::app::state::DEFAULT_FORGE_ICON));
        assert!(!line("website").contains(crate::app::state::DEFAULT_FORGE_ICON));
        // The row without a mark still starts where the others do — a ragged
        // left edge is harder to read down a list than a repeated glyph.
        // Counted in columns, not bytes: the glyph is three bytes wide and one
        // column, which is the whole reason it lines up at all.
        let col = |name: &str| {
            let l = line(name);
            l[..l.find(name).unwrap()].chars().count()
        };
        assert_eq!(col("backend"), col("website"));
    }

    #[test]
    fn a_terminal_without_the_font_can_turn_the_marks_off() {
        let mut st = dashboard_with_live_ci();
        st.forge_icon = String::new();
        let out = draw_repos(&st, 150, 12);
        assert!(!out.contains(crate::app::state::DEFAULT_FORGE_ICON));
        // And the width it was taking goes back to the names.
        let with_icon = draw_repos(&dashboard_with_live_ci(), 150, 12);
        let col = |s: &str| {
            let l = s.lines().find(|l| l.contains("backend")).unwrap();
            l[..l.find("muufree").unwrap()].chars().count()
        };
        assert!(col(&out) < col(&with_icon));
    }

    /// A dashboard scanned out of a deep workspace path, one repo dirty.
    fn workspace_dashboard() -> AppState {
        let mut st = dashboard_with_live_ci();
        st.workspace_root = Some(std::path::PathBuf::from("/data/repos/muufree/github"));
        st.repos[1].git = Some(crate::git::parse_status("## main\0M  a.rs\0 M b.rs\0 M c.rs\0"));
        st
    }

    #[test]
    fn the_header_carries_how_the_workspace_stands() {
        let out = draw_header(&workspace_dashboard(), 150);
        // The counts the dashboard's title bar used to hold — up a line, so
        // they are still there when you are three views deep in a run's logs
        // and a second repo goes red behind you.
        assert!(out.contains("✓1"), "{out}");
        assert!(out.contains("⏵2"), "{out}");
        assert!(out.contains("◆3 uncommitted"), "{out}");
        // The total comes up with them: a repo that has never run CI is in none
        // of the four tallies, so they cannot be added up into it.
        assert!(out.contains("3 repos"), "{out}");
    }

    #[test]
    fn the_dashboard_wears_no_frame_restating_its_own_header() {
        // Nothing in flight, so the Live strip — which keeps its own frame,
        // because it is a thing arriving rather than the view itself — is not
        // on screen to be confused for the table's.
        let mut st = workspace_dashboard();
        st.run_progress.clear();
        let out = draw_repos(&st, 150, 10);
        // The box was two rows and two columns spent saying what the line above
        // it already says.
        assert!(!out.contains("Repos  "), "got:\n{out}");
        assert!(!out.contains('╭') && !out.contains('╰'), "got:\n{out}");
        // The first row of the view is the column header, not a border.
        assert!(out.lines().next().unwrap().contains("Repo "), "got:\n{out}");
    }

    #[test]
    fn a_zero_is_not_bad_news() {
        let mut st = workspace_dashboard();
        for card in st.repos.iter_mut() {
            card.runs.clear();
            card.error = None;
        }
        st.run_progress.clear();
        let theme = Theme::default();
        let spans = workspace_tallies(&st);
        let fail = spans
            .iter()
            .find(|s| s.content.contains('✗'))
            .expect("the fail count is always shown");
        // Painting ✗0 in failure red puts a red mark in the corner of a
        // workspace where nothing is wrong — and teaches you to stop reading
        // the colour that is supposed to mean something.
        assert_eq!(fail.style.fg, Some(theme.text_faint), "{:?}", fail.content);
        assert_ne!(fail.style.fg, Some(theme.failure));
    }

    #[test]
    fn the_header_gives_up_the_middle_before_it_collides() {
        // Narrow enough that all three blocks cannot coexist. Shedding from the
        // widest, least load-bearing figure inwards reads as a small window;
        // letting the two ends meet reads as a bug.
        let out = draw_header(&workspace_dashboard(), 78);
        assert!(!out.contains("uncommitted"), "{out}");
        let now = Local::now().format("%H:%M").to_string();
        assert!(out.contains(&now), "the clock survives: {out}");
        assert!(out.contains("github"), "the path survives: {out}");
    }

    #[test]
    fn the_middle_of_the_header_says_what_is_happening_right_now() {
        let out = draw_header(&workspace_dashboard(), 150);
        // Two runs in flight: name the first and count the rest, rather than
        // growing the header into a list.
        assert!(out.contains("backend"), "{out}");
        assert!(out.contains("Deploy to Stage"), "{out}");
        assert!(out.contains("+1"), "{out}");

        // Nothing running, nothing to say — the slot empties rather than
        // holding a stale line.
        let mut idle = workspace_dashboard();
        idle.run_progress.clear();
        assert!(!draw_header(&idle, 150).contains("Deploy to Stage"));
    }

    #[test]
    fn trouble_outranks_progress_in_the_middle() {
        let mut st = dashboard_out_of_quota();
        st.quota = Some(quota_at(99, Utc::now() + chrono::Duration::minutes(12)));
        let at = Local::now() + chrono::Duration::minutes(12);
        let out = draw_header(&st, 150);
        // Three rows can only repeat "rate limited". The header is where there
        // is room to say when it stops being true.
        assert!(out.contains(&format!("retry {}", at.format("%H:%M"))), "{out}");
    }

    #[test]
    fn a_deep_path_keeps_only_the_part_that_says_which_checkout() {
        let p = std::path::Path::new("/data/repos/muufree/github");
        assert_eq!(short_path(p), "…/muufree/github");
        // Short enough to be worth saying in full.
        assert_eq!(short_path(std::path::Path::new("/srv/code")), "/srv/code");
        // Home is a prefix everyone already knows the expansion of.
        if let Some(home) = dirs::home_dir() {
            let deep = home.join("a/b/c/d");
            assert_eq!(short_path(&deep), "…/c/d");
            assert_eq!(short_path(&home.join("work")), "~/work");
        }
    }

    #[test]
    fn a_broken_row_shows_the_cause_not_the_call_that_hit_it() {
        let out = draw_repos(&dashboard_out_of_quota(), 150, 12);
        // The whole complaint: the cell is ~20 columns wide, so spending them
        // on the call we made ("list all repo runs: Gi…") says nothing at all.
        assert!(out.contains("rate limited"), "{out}");
        assert!(!out.contains("list all repo runs"), "{out}");
    }

    #[test]
    fn one_shared_cause_is_stated_once_with_the_clock_attached() {
        let mut st = dashboard_out_of_quota();
        st.quota = Some(quota_at(40, Utc::now() + chrono::Duration::minutes(12)));
        let at = Local::now() + chrono::Duration::minutes(12);
        // Three rows can only repeat "rate limited". The header is where there
        // is room to say when it stops being true.
        assert!(
            draw_header(&st, 150).contains(&format!("retry {}", at.format("%H:%M"))),
            "{}",
            draw_header(&st, 150)
        );

        // A reset that has already passed is worse than none — it dates the
        // screen to a moment that is over.
        st.quota = Some(quota_at(40, Utc::now() - chrono::Duration::minutes(1)));
        assert!(!draw_header(&st, 150).contains("retry"));
    }

    #[test]
    fn the_pace_limit_does_not_borrow_the_hourly_clock() {
        // The screenshot this comes from: a meter reading "API 4%" beside eight
        // rows saying "rate limited", and a retry time an hour away. Two
        // different limits were wearing one word — the hourly budget was barely
        // touched, and what jog had actually run into was the pace limit, which
        // is over in a minute.
        let mut st = dashboard_out_of_quota();
        for card in st.repos.iter_mut() {
            card.error = Some(ApiError {
                fault: ApiFault::Throttled,
                text: ApiFault::Throttled.label().into(),
            });
        }
        st.quota = Some(quota_at(4, Utc::now() + chrono::Duration::minutes(55)));
        st.hold_api(None);

        let out = draw_header(&st, 150);
        assert!(out.contains("API 4%"), "{out}");
        assert!(out.contains("asked too fast"), "{out}");
        // The hour's reset is not the answer to this one, and offering it sends
        // you away for fifty-five minutes over a fifteen-second wait.
        let hour_away = (Local::now() + chrono::Duration::minutes(55)).format("%H:%M");
        assert!(!out.contains(&hour_away.to_string()), "{out}");
        assert!(out.contains("retrying in"), "{out}");
    }

    #[test]
    fn rows_that_broke_differently_get_no_shared_headline() {
        let mut st = dashboard_out_of_quota();
        st.repos[1].error = Some(ApiError {
            fault: ApiFault::NotFound,
            text: ApiFault::NotFound.label().into(),
        });
        // No one line is true of all three, so the header stays at the count
        // and lets the rows disagree in their own cells.
        assert!(shared_fault_detail(&st).is_none());
        let out = draw_repos(&st, 150, 12);
        assert!(out.contains("rate limited") && out.contains("not found"), "{out}");
    }

    #[test]
    fn a_row_still_loading_says_so_by_moving() {
        let mut st = dashboard_with_live_ci();
        st.repos[0].loaded = false;
        st.repos[0].runs.clear();

        // Two frames far enough apart that the sweep has to have moved.
        st.tick_count = 0;
        let a = draw_repos(&st, 150, 12);
        st.tick_count = 6;
        let b = draw_repos(&st, 150, 12);
        let row = |out: &str| {
            out.lines()
                .find(|l| l.contains("muufree/backend"))
                .unwrap()
                .to_string()
        };
        assert_ne!(row(&a), row(&b), "a still image cannot say 'still waiting'");
        assert!(row(&a).contains('─'), "got {:?}", row(&a));
    }

    #[test]
    fn nothing_running_means_no_strip_at_all() {
        let mut st = dashboard_with_live_ci();
        st.run_progress.clear();
        let out = draw_repos(&st, 150, 16);
        assert!(!out.contains("in flight"), "got:\n{out}");
        // …and the table gets the space back.
        assert!(out.contains("muufree/website"), "got:\n{out}");
    }

    #[test]
    fn a_short_terminal_keeps_the_table_over_the_strip() {
        // The list is the view. On a screen too short for both, the strip is
        // the part that yields — losing repo rows to a status panel is worse
        // than not knowing which step a deploy is on.
        let out = draw_repos(&dashboard_with_live_ci(), 150, 7);
        assert!(!out.contains("in flight"), "got:\n{out}");
        for spec in ["muufree/backend", "muufree/cms", "muufree/website"] {
            assert!(out.contains(spec), "{spec} missing from:\n{out}");
        }
        // And the two rows the frame used to eat now buy the strip its place on
        // a screen that could not hold both before.
        let taller = draw_repos(&dashboard_with_live_ci(), 150, 9);
        assert!(taller.contains("in flight"), "got:\n{taller}");
        assert!(taller.contains("muufree/website"), "got:\n{taller}");
    }

    /// What one whole frame costs to build.
    ///
    /// ratatui keeps no retained widget tree: every `Row`, `Span` and `String`
    /// on screen is rebuilt from nothing on every draw, and the cell diff only
    /// saves terminal writes, not this. So the frame clock multiplies whatever
    /// this number is — which is the only thing that decides whether jog can
    /// afford to animate at 30fps.
    #[test]
    #[ignore = "measurement: cargo test frame_cost -- --ignored --nocapture"]
    fn frame_cost() {
        let mut st = dashboard_with_live_ci();
        // A workspace the size of a real one, not the three-row fixture.
        while st.repos.len() < 8 {
            let n = st.repos.len();
            let mut card = crate::app::state::RepoCard::new(format!("muufree/repo{n}"));
            card.git = Some(crate::git::parse_status("## main\0M  a.rs\0 M b.rs\0"));
            card.loaded = true;
            card.runs = (0..20)
                .map(|i| a_run(100 + i, "CodeQL Security Risk", Status::Success, 400))
                .collect();
            st.repos.push(card);
        }
        st.workspace_root = Some(std::path::PathBuf::from("/data/repos/muufree/github"));

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 50)).unwrap();
        // Warm the buffers so the first-frame allocations are not in the sample.
        for _ in 0..50 {
            term.draw(|f| render(f, &st)).unwrap();
        }
        const N: u32 = 2000;
        let t0 = std::time::Instant::now();
        for i in 0..N {
            // Vary the animation clock so nothing is trivially cached and the
            // cell diff has real work to do, as it would on a live screen.
            st.tick_count = i as u64;
            term.draw(|f| render(f, &st)).unwrap();
        }
        let per = t0.elapsed() / N;
        println!(
            "{} repos, 200x50: {:?}/frame → {:.2}% of one core at 10fps, {:.2}% at 30fps",
            st.repos.len(),
            per,
            per.as_secs_f64() * 10.0 * 100.0,
            per.as_secs_f64() * 30.0 * 100.0,
        );
    }

    /// What the colour maths behind a fade costs, against the frame it rides in.
    ///
    /// The unit of "make it prettier without animating it" is one `mix()` and
    /// one `Style` per cell. If a whole screen of that disappears into the frame
    /// jog is already drawing ten times a second, the effect is free and the
    /// only question left is whether it looks good.
    #[test]
    #[ignore = "measurement: cargo test blend_cost -- --ignored --nocapture"]
    fn blend_cost() {
        let theme = Theme::default();
        const CELLS: u32 = 200 * 50; // a full 200x50 screen
        const REPS: u32 = 200;
        let t0 = std::time::Instant::now();
        let mut sink = 0u32;
        for r in 0..REPS {
            for i in 0..CELLS {
                let t = (i % 100) as f64 / 100.0;
                let c = mix(theme.success, theme.failure, t);
                let s = Style::default().fg(c).bg(theme.surface);
                // Defeat the optimiser without adding measurable work.
                sink = sink.wrapping_add(s.fg.is_some() as u32).wrapping_add(r);
            }
        }
        let per_screen = t0.elapsed() / REPS;
        println!(
            "blend+style, {CELLS} cells: {:?}/screen ({} sink)",
            per_screen, sink
        );
    }

    #[test]
    #[ignore = "visual check: cargo test show_header -- --ignored --nocapture"]
    fn show_header() {
        let st = workspace_dashboard();
        for w in [150u16, 120, 100, 78] {
            println!("─── {w} cols ───\n{}", draw_header(&st, w));
        }
        println!("─── dashboard, 150×10 ───\n{}", draw_repos(&st, 150, 10));
    }

    #[test]
    #[ignore = "visual check: cargo test show_live_strip -- --ignored --nocapture"]
    fn show_live_strip() {
        let mut st = dashboard_with_live_ci();
        for tick in [0u64, 6, 12] {
            st.tick_count = tick;
            println!("─── tick {tick} ───\n{}", draw_repos(&st, 150, 16));
        }
    }

    #[test]
    fn a_guessed_active_repo_is_not_marked_active() {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        for spec in ["acme/api", "acme/web"] {
            let mut card = crate::app::state::RepoCard::new(spec.into());
            card.path = Some(std::path::PathBuf::from("/tmp").join(spec));
            card.git = Some(crate::git::parse_status("## main\0"));
            card.loaded = true;
            st.repos.push(card);
        }
        st.view = View::Repos;

        let draw = |st: &AppState| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 10)).unwrap();
            term.draw(|f| render_repos(f, f.area(), st)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .find(|r| r.contains("acme/api"))
                .unwrap()
        };

        // Entered deliberately: the dot says which repo we're on.
        assert!(draw(&st).contains("acme/api  ●"), "got {:?}", draw(&st));

        // Guessed at startup from a workspace scan: nothing to point at.
        st.repo_label_implicit = true;
        assert!(!draw(&st).contains("●"), "got {:?}", draw(&st));
    }

    #[test]
    fn no_op_means_no_pane() {
        let st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let out = draw_changes(&st, 80, 14);
        assert!(!out.contains("hook"), "got:\n{out}");
    }

    #[test]
    fn diff_headers_are_not_mistaken_for_content() {
        let theme = Theme::default();
        let fg = |s: &str| diff_line_style(s, &theme).fg.unwrap();
        // `+++`/`---` open with the same characters as an added/removed line but
        // are file headers; colouring them green/red fakes a change per file.
        let meta = theme.text_muted;
        assert_eq!(fg("+++ b/src/main.rs"), meta);
        assert_eq!(fg("--- a/src/main.rs"), meta);
        assert_eq!(fg("diff --git a/x b/x"), meta);
        assert_eq!(fg("Binary files a/q.png and b/q.png differ"), meta);

        assert_eq!(fg("+let x = 1;"), theme.success);
        assert_eq!(fg("-let x = 0;"), theme.failure);
        assert_eq!(fg("@@ -1,4 +1,4 @@"), theme.accent);
        // A context line keeps the neutral body colour, blank lines included.
        assert_eq!(fg(" unchanged"), theme.text);
        assert_eq!(fg(""), theme.text);
    }

    #[test]
    fn every_view_maps_to_a_real_help_section() {
        let km = crate::config::KeymapConfig::default();
        let titles: Vec<&str> = help_sections(&km).iter().map(|(t, _)| *t).collect();
        // Guards against adding a View and forgetting its help section.
        for view in [
            View::Repos,
            View::GitStatus,
            View::GitDiff,
            View::Workflows,
            View::Runs,
            View::Watch,
            View::RunDetail,
            View::Diff,
            View::Logs,
            View::TriggerPrompt,
        ] {
            let section = help_section_for(view);
            assert!(
                titles.contains(&section),
                "{view:?} maps to `{section}`, which is not a help section"
            );
        }
    }

    /// Draw the help overlay into an off-screen terminal and return it as text.
    fn draw_help(state: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_help_overlay(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_wide_terminal_gets_the_help_in_two_columns_without_scrolling() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.view = View::Logs;
        st.show_help = true;
        let text = draw_help(&st, 120, 54);
        // Everything is on screen: nothing left to scroll to, and both the
        // first and the last section are visible at once.
        assert!(text.contains("any key closes"), "no scroll footer:\n{text}");
        assert!(text.contains("Global"));
        assert!(text.contains("Trigger prompt"));
        // The current view's section floats to the top and wears the mark.
        assert!(text.contains("you are here"));
        let here_line = text.lines().find(|l| l.contains("you are here")).unwrap();
        assert!(here_line.contains("Logs"), "the mark sits on the open view's section: {here_line}");
        // Two columns: some row shares a line with content in the right half.
        let two_col = text.lines().any(|l| {
            let half = l.len() / 2;
            l.len() > 60 && !l[..half].trim().is_empty() && !l[half..].trim().is_empty()
        });
        assert!(two_col, "expected side-by-side sections:\n{text}");
    }

    #[test]
    fn a_narrow_terminal_keeps_one_readable_column_and_can_scroll() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.view = View::Repos;
        st.show_help = true;
        let text = draw_help(&st, 60, 20);
        assert!(text.contains("scroll"), "a clipped overlay says how to scroll:\n{text}");
        // Long descriptions wrap instead of running off the dialog edge.
        assert!(
            text.lines().all(|l| l.chars().count() <= 60),
            "no line escapes the terminal:\n{text}"
        );
    }

    #[test]
    fn help_reflects_remapped_keys() {
        let km = crate::config::KeymapConfig {
            git_commit: "x".into(),
            ..Default::default()
        };
        let sections = help_sections(&km);
        let changes = sections
            .iter()
            .find(|(t, _)| *t == "Changes — working tree")
            .expect("changes section");
        let commit = changes
            .1
            .iter()
            .find(|(_, d)| d.starts_with("commit"))
            .expect("commit row");
        assert_eq!(commit.0, "x", "help must show the configured key, not the default");
    }

    #[test]
    fn help_sections_are_all_populated() {
        let km = crate::config::KeymapConfig::default();
        for (title, rows) in help_sections(&km) {
            assert!(!rows.is_empty(), "`{title}` has no rows");
        }
    }

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
