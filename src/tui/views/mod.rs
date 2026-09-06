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
    AppState, BatchPhase, ByteSpan, DetailItem, DiffLine, DiffRow, DiffSide, GitDiffView, GitOp,
    Hit, ItemState,
    StatusKind, Theme, View, ansi_line_to_spans, build_detail_items,
};
use crate::history::HistoryEntry;
use crate::provider::github::{ApiFault, CRITICAL_PERCENT};
use crate::provider::{Job, Run, Status};

pub fn render(f: &mut Frame, state: &AppState) {
    let area = f.area();
    // The click map describes the frame being drawn now, not any earlier one.
    state.hits.borrow_mut().clear();
    state.last_frame_width.set(area.width);
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
    render_services_overlay(f, area, state);
    // Drawn last so it sits above every other overlay.
    render_help_overlay(f, area, state);
}

/// The services card's entrance, in ticks at the app's 100ms clock.
///
/// A card that arrives complete asks to be read all at once; one whose rows
/// land in sequence hands the eye an order to read them in. Each row waits
/// `REVEAL_STEP` ticks longer than the one above it, then brings its verdict
/// up out of the card's own background over `REVEAL_TICKS`. Eight monitors
/// are done inside a second: fast enough that the card is readable about as
/// soon as the eye reaches it, slow enough to still read as one movement
/// down the card rather than a flash.
///
/// The step is fractional on purpose. A stagger has to be shorter than a tick
/// to keep a sweep this quick from collapsing into every row landing at once,
/// and the fade each row plays is long enough that a sub-tick offset between
/// two of them still shows up as two different frames.
const REVEAL_STEP: f64 = 0.6;
const REVEAL_TICKS: f64 = 3.0;

/// How far row `i` is into that entrance, 0.0 → 1.0.
///
/// 1.0 when no opening tick was recorded: the card is on screen without the
/// clock having been started (a test, a direct `show_services`), and a settled
/// card is the honest answer there rather than a frozen first frame.
fn reveal_at(state: &AppState, i: usize) -> f64 {
    staggered_reveal(state.services_opened_tick, state.tick_count, i, REVEAL_STEP, REVEAL_TICKS)
}

/// How far row `i` of a card opened at `opened` is into its entrance,
/// 0.0 → 1.0 — the one clock behind every card that arrives row by row.
fn staggered_reveal(opened: Option<u64>, tick: u64, i: usize, step: f64, ticks: f64) -> f64 {
    let Some(opened) = opened else {
        return 1.0;
    };
    let since = tick.saturating_sub(opened) as f64;
    let delay = i as f64 * step;
    ((since - delay) / ticks).clamp(0.0, 1.0)
}

/// Whether the card still has rows arriving — the redraw loop's question.
pub fn services_revealing(state: &AppState) -> bool {
    state.show_services && reveal_at(state, state.services.len()) < 1.0
}

/// The help card's entrance: the same sweep as the services card's, at a
/// brisker step — a reference has several dozen rows where the services card
/// has eight, and the same pace would hold the bottom half hostage.
const HELP_REVEAL_STEP: f64 = 0.25;
const HELP_REVEAL_TICKS: f64 = 3.0;

/// A generous ceiling on how long that entrance can run, in ticks — the
/// redraw loop's question, answered without measuring the card's content.
const HELP_REVEAL_HORIZON: u64 = 25;

fn help_reveal_at(state: &AppState, i: usize) -> f64 {
    staggered_reveal(
        state.help_opened_tick,
        state.tick_count,
        i,
        HELP_REVEAL_STEP,
        HELP_REVEAL_TICKS,
    )
}

/// Whether the help card is still mid-entrance.
pub fn help_revealing(state: &AppState) -> bool {
    state.show_help
        && state
            .help_opened_tick
            .is_some_and(|o| state.tick_count.saturating_sub(o) < HELP_REVEAL_HORIZON)
}

/// The dashboard's entrance: the same sweep the cards play, on the view you
/// actually land on. A step between the services card's and the help card's —
/// a dashboard is a handful of rows on most screens but forty on a wall
/// monitor, and a pace that reads as one movement down eight rows would leave
/// the bottom of a long list arriving well after the eye got there.
const DASH_REVEAL_STEP: f64 = 0.35;
const DASH_REVEAL_TICKS: f64 = 3.0;

/// How far screen line `i` of the table is into that entrance, 0.0 → 1.0.
///
/// Indexed by line on screen rather than by repo: the sweep is "the table
/// arrives from the top", so a scrolled list still fills from its first
/// visible row instead of starting part-way through its own animation.
fn dash_reveal_at(state: &AppState, i: usize) -> f64 {
    staggered_reveal(
        state.dash_opened_tick,
        state.tick_count,
        i,
        DASH_REVEAL_STEP,
        DASH_REVEAL_TICKS,
    )
}

/// Lines the table's header occupies: the labels, plus the blank one its
/// `bottom_margin` puts between them and the first repo.
const HEADER_LINES: u16 = 2;

/// Whether the dashboard still has rows arriving — the redraw loop's question.
///
/// Bounded by the repo count plus the header and a few owner headings, so a
/// long list keeps the loop at full rate exactly as long as its own sweep runs.
pub fn dash_revealing(state: &AppState) -> bool {
    state.view == View::Repos && dash_reveal_at(state, state.repos.len() + 4) < 1.0
}

/// How long the footer's `? help` beacon breathes after startup: ten seconds
/// at the 100ms tick, or until help is opened, whichever comes first.
pub(super) const HELP_BEACON_TICKS: u64 = 100;

/// The chip colour for an environment's name.
///
/// Environments are read at a glance or not at all, and the glance is the
/// colour: green is the one you don't touch, amber the one you do. Names
/// nobody has a convention for still get a chip — an unlabelled section reads
/// as a rendering failure, not as "no environment".
fn env_color(label: &str, theme: &Theme) -> Color {
    let lower = label.trim().to_ascii_lowercase();
    let head = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|w| !w.is_empty())
        .unwrap_or("");
    if head.starts_with("prod") || head == "prd" || head == "live" {
        theme.success
    } else if head.starts_with("stag") || head == "stg" || head.starts_with("pre") {
        theme.warning
    } else if head.starts_with("dev") || head == "local" {
        theme.info
    } else if head.starts_with("test") || head == "qa" || head.starts_with("sandbox") {
        theme.primary
    } else if head.is_empty() || head == "untagged" {
        theme.unknown
    } else {
        theme.accent
    }
}

/// A chip's own two colours: the fill, deepened until label text can sit on
/// it, and the ink that reads against that fill.
///
/// The palette's `success` is a pastel meant for glyphs on a dark panel; used
/// flat as a background it would wash out anything written over it.
fn chip_colors(base: Color) -> (Color, Color) {
    let fill = mix(base, Color::Rgb(0, 0, 0), 0.28);
    let ink = match fill {
        // Rec. 601 luma — green carries most of the perceived brightness, so
        // a plain channel average picks the wrong ink on amber.
        Color::Rgb(r, g, b) => {
            let luma = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
            if luma > 150.0 { Color::Black } else { Color::White }
        }
        // 256-colour terminals never got the darkening (an indexed colour has
        // no arithmetic), so the fill is still the pastel: dark ink on it.
        _ => Color::Black,
    };
    (fill, ink)
}

/// One environment heading, drawn as a filled chip.
///
/// The half-blocks are the rounded ends: `▐` fills the right half of its cell
/// and `▌` the left half of its own, so the fill runs off into a soft cap at
/// each end instead of a hard rectangle. Block Elements, not Powerline — the
/// shape survives a terminal without a patched font.
fn env_chip(label: &str, p: f64, theme: &Theme) -> Vec<Span<'static>> {
    let (fill, ink) = chip_colors(env_color(label, theme));
    // Chips fade up out of the card the way their rows do, a step ahead of
    // the first of them.
    let fill = mix(theme.surface_alt, fill, p);
    let ink = mix(fill, ink, p);
    vec![
        Span::styled("▐", Style::default().fg(fill)),
        Span::styled(
            format!(" {label} "),
            Style::default().bg(fill).fg(ink).bold(),
        ),
        Span::styled("▌", Style::default().fg(fill)),
    ]
}

/// Every monitor by name — the card behind the header's heart.
///
/// The tally can only count and the Live column only decorates mapped rows;
/// this is where "which ones, and how are they doing" gets answered, the
/// unmapped monitors included. A card, not a view: nothing here is acted on,
/// so any key puts it away.
fn render_services_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    if !state.show_services {
        return;
    }
    let theme = &state.theme;

    let rows: Vec<Line> = if state.services.is_empty() {
        vec![Line::from(Span::styled(
            "nothing to show — add [uptime_kuma] with your status page's URL to config.toml",
            Style::default().fg(theme.text_muted).italic(),
        ))]
    } else {
        let name_w = state
            .services
            .iter()
            .map(|s| disp_width(&s.name))
            .max()
            .unwrap_or(4);
        // What divides the card into sections, best evidence first: the status
        // page's own groups when it has more than one; failing that, the
        // monitors' first tags — five rows each wearing the same Prod chip
        // are a group that hasn't been drawn yet. One lone group under a card
        // already titled "Services" would just say the same word twice.
        let distinct_groups: std::collections::HashSet<&str> =
            state.services.iter().map(|s| s.group.as_str()).collect();
        let by_group = distinct_groups.len() > 1;
        let by_tag = !by_group && state.services.iter().any(|s| !s.tags.is_empty());
        let mut sections: Vec<(String, Vec<&crate::kuma::Service>)> = Vec::new();
        for s in &state.services {
            let key = if by_group {
                s.group.clone()
            } else if by_tag {
                s.tags.first().cloned().unwrap_or_default()
            } else {
                String::new()
            };
            match sections.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(s),
                None => sections.push((key, vec![s])),
            }
        }
        // The untagged straggle in at the end rather than opening the card.
        if by_tag && let Some(i) = sections.iter().position(|(k, _)| k.is_empty()) {
            let untagged = sections.remove(i);
            sections.push(untagged);
        }

        let mut out: Vec<Line> = Vec::new();
        // Counted across sections, not within one: the reveal is a single
        // sweep down the card, and a per-section counter would restart it at
        // every heading.
        let mut row = 0usize;
        for (i, (section, members)) in sections.iter().enumerate() {
            if sections.len() > 1 {
                if i > 0 {
                    out.push(Line::from(""));
                }
                let label = if section.is_empty() { "untagged" } else { section };
                let mut spans = vec![Span::raw(" ")];
                spans.extend(env_chip(label, reveal_at(state, row), theme));
                out.push(Line::from(spans));
            }
            for s in members {
                let p = reveal_at(state, row);
                row += 1;
                out.push({
                    use crate::kuma::ServiceState;
                    let (glyph, color, word) = match s.state {
                        ServiceState::Up => ("●", theme.success, "up"),
                        ServiceState::Down => ("✗", theme.failure, "down"),
                        ServiceState::Pending => ("◌", theme.warning, "pending"),
                        ServiceState::Maintenance => ("◒", theme.info, "maintenance"),
                    };
                    // The verdict grows into place: a dot out of the card's
                    // own background, then the mark itself, then its word
                    // written after it. The name is at full strength from the
                    // first frame, so the card's shape never jumps.
                    let mark = if p < 0.45 { "·" } else { glyph };
                    let shown = (word.len() as f64 * p).ceil() as usize;
                    let word = &word[..shown.min(word.len())];
                    let mut style = Style::default().fg(mix(theme.surface_alt, color, p));
                    if s.state == ServiceState::Down {
                        style = style.bold();
                    }
                    let fade = |c: Color| Style::default().fg(mix(theme.surface_alt, c, p));
                    let mut spans = vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("{:<name_w$}", s.name),
                            Style::default().fg(theme.text_bright),
                        ),
                        // The dot rides with its word: colour and text answer
                        // the same question in the same place.
                        Span::styled(format!("  {mark} {word:<12}"), style),
                        Span::styled(
                            match s.ping_ms {
                                Some(p) => format!("{p:>5}ms"),
                                None => format!("{:>7}", "—"),
                            },
                            fade(theme.text),
                        ),
                        Span::styled(
                            match s.uptime24 {
                                Some(u) => format!("  {:>5.1}% today", u * 100.0),
                                None => String::new(),
                            },
                            fade(theme.text_muted),
                        ),
                    ];
                    // Tags, when the status page publishes them ("Show Tags"
                    // in Kuma's page settings) — minus the one already serving
                    // as this section's heading, which a chip would repeat.
                    for tag in s.tags.iter().filter(|t| *t != section) {
                        spans.push(Span::styled(format!("  #{tag}"), fade(theme.info)));
                    }
                    // Which dashboard row this monitor decorates, when one does.
                    if let Some(repo) = state.service_repos.get(&s.name) {
                        spans.push(Span::styled(format!("  → {repo}"), fade(theme.text_faint)));
                    }
                    Line::from(spans)
                });
            }
        }
        out
    };

    let w = rows
        .iter()
        .map(|l| l.width() as u16 + 4)
        .max()
        .unwrap_or(40)
        .clamp(36, area.width.saturating_sub(4));
    // Borders, plus a blank line above the first row and below the last: the
    // chips are filled shapes, and a chip that touches the frame reads as part
    // of it rather than as something sitting inside it.
    let h = (rows.len() as u16 + 4).min(area.height.saturating_sub(2));
    let slot = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, slot);
    // Its own border, brighter than the panels': every other frame on screen
    // sits *under* things, while this one floats above them all and dim grey
    // read as part of the background it was covering.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .padding(Padding::vertical(1))
        .title(Line::from(vec![
            Span::styled("─┤ ", Style::default().fg(theme.accent)),
            Span::styled("♥ Services", Style::default().fg(theme.accent).bold()),
            Span::styled(" ├", Style::default().fg(theme.accent)),
        ]))
        .title_bottom(
            Line::from(Span::styled(
                " r refresh · any other key closes ",
                Style::default().fg(theme.text_faint),
            ))
            .right_aligned(),
        );
    // When these readings were fetched — the one fact that separates "all up"
    // from "all up, as of some time before the wifi dropped".
    let block = match state.kuma_fetched_at {
        Some(at) => {
            let secs = (Utc::now() - at).num_seconds().max(0);
            let ago = if secs < 60 {
                format!("{secs}s ago")
            } else {
                format!("{}m ago", secs / 60)
            };
            block.title_bottom(Line::from(Span::styled(
                format!(" updated {ago} "),
                Style::default().fg(theme.text_faint),
            )))
        }
        None => block,
    };
    // Solid ground like the help card's: Clear alone leaves default cells,
    // which a translucent terminal renders as wallpaper behind the text.
    f.render_widget(
        Paragraph::new(rows)
            .block(block)
            .style(Style::default().bg(theme.surface_alt)),
        slot,
    );
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
                // The file the viewport is scrolled into — the crumb follows
                // the reading position through the combined diff.
                state
                    .git_diff
                    .as_ref()
                    .map(|d| d.current_file().unwrap_or_else(|| d.file.clone()))
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
        let mut t = Vec::new();
        // The bell wears the truth about notifications: quietly lit while a
        // landing run would make noise, slashed with the countdown while
        // snoozed. Muted is a state worth wearing — a silence that looks like
        // "nothing failed" is the one lie this header must not tell. No bell
        // at all when config never announces: a permanently slashed bell
        // would nag about a choice already made.
        if state.notify_enabled {
            if let Some(until) = state.snooze_until.filter(|t| *t > Utc::now()) {
                let mins = ((until - Utc::now()).num_seconds() + 59) / 60;
                // An empty off-glyph still says "muted" in words: hiding the
                // bell is a look, hiding the mute is a trap.
                let icon = if state.bell_off_icon.is_empty() {
                    "muted"
                } else {
                    state.bell_off_icon.as_str()
                };
                t.push(Span::styled(
                    format!("{icon} {mins}m   "),
                    Style::default().fg(theme.warning),
                ));
            } else if !state.bell_icon.is_empty() {
                t.push(Span::styled(
                    format!("{}   ", state.bell_icon),
                    Style::default().fg(theme.text_faint),
                ));
            }
        }
        t.extend(quota_spans(state));
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

    // The moment the last in-flight run lands and every row is green, one
    // comet of that green crosses the header and is gone. Painted over the
    // finished cells rather than rendered as a widget, so it lights the text
    // it passes instead of replacing it — a sweep, not a banner.
    if let Some(at) = state.all_green_tick {
        let strength = Motion::new(state.tick_count).decay(at, CELEBRATE_TICKS);
        if strength > 0.0 {
            const TAIL: f64 = 12.0;
            // The head enters at the left edge and exits fully, tail and all.
            let head = (1.0 - strength) * (area.width as f64 + TAIL);
            let buf = f.buffer_mut();
            for x in 0..area.width {
                let behind = head - x as f64;
                if (0.0..TAIL).contains(&behind) {
                    let t = (1.0 - behind / TAIL) * 0.45;
                    let cell = &mut buf[(area.x + x, area.y)];
                    let bg = cell.style().bg.unwrap_or(theme.surface);
                    cell.set_bg(mix(bg, theme.success, t));
                }
            }
        }
    }
}

/// The down-banner's heart, mid-beat: lub, dub, rest, on the 100ms tick.
///
/// Filled and bright on the two beats, hollow and dim between — motion in the
/// corner of the eye, so a down service is noticed without being read. The
/// words next to it hold still and stay legible.
fn beating_heart(tick: u64, theme: &Theme) -> (&'static str, Color) {
    match tick % 12 {
        0 | 1 | 4 | 5 => ("♥ ", theme.failure),
        _ => ("♡ ", theme.failure_dim),
    }
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
    // A service being down is the same rank of trouble: production not
    // answering outranks anything CI is merely doing.
    let down: Vec<&crate::kuma::Service> = state
        .services
        .iter()
        .filter(|s| s.state == crate::kuma::ServiceState::Down)
        .collect();
    if let Some(first) = down.first() {
        let (heart, heart_fg) = beating_heart(state.tick_count, theme);
        let mut out = vec![
            Span::styled(heart, Style::default().fg(heart_fg).bold()),
            Span::styled(
                format!("{} down", first.name),
                Style::default().fg(theme.failure).bold(),
            ),
        ];
        if down.len() > 1 {
            out.push(Span::styled(
                format!("  +{}", down.len() - 1),
                Style::default().fg(theme.failure),
            ));
        }
        return out;
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
    // The stopwatch with the answer attached, when this workflow's recent
    // durations offer one.
    if let Some(t) = typical_run_secs(card.runs.iter(), &detail.run.display_title) {
        let secs = (Utc::now() - detail.run.created_at).num_seconds().max(0);
        out.push(Span::styled(
            format!(" · {}", eta_text(t, secs)),
            Style::default().fg(theme.text_faint),
        ));
    }
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
    let mut out = Vec::new();
    // Service health, when an Uptime Kuma page is configured: quiet while
    // everything answers, loud the moment something doesn't — CI can be all
    // green while production is down, and that is the one combination this
    // corner must never be calm about.
    if !state.services.is_empty() {
        let total = state.services.len();
        let down = state
            .services
            .iter()
            .filter(|s| s.state == crate::kuma::ServiceState::Down)
            .count();
        out.push(if down > 0 {
            Span::styled(
                format!("♥{}/{}", total - down, total),
                Style::default().fg(theme.failure).bold(),
            )
        } else {
            Span::styled(format!("♥{total}"), Style::default().fg(theme.success_dim))
        });
        out.push(Span::styled("   ", Style::default()));
    }
    let t = Tallies::of(state);
    if state.repos.is_empty() {
        return out;
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
    out.push(count(t.ok, format!("✓{}", t.ok), theme.success, false));
    out.push(count(t.fail, format!("  ✗{}", t.fail), theme.failure, false));
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
    // Windows displays `\`, but the header always speaks `/` — normalising
    // here also keeps the home-prefix match and the split below in one idiom.
    let norm = |s: String| {
        if cfg!(windows) {
            s.replace('\\', "/")
        } else {
            s
        }
    };
    let full = norm(p.display().to_string());
    let shown = match dirs::home_dir().map(|h| norm(h.display().to_string())) {
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
            hints.push((display_key(&km.refresh).into(), "refresh"));
            hints.push((display_key(&km.finder).into(), "find"));
            // Advertised only while there is something behind it.
            if !state.services.is_empty() {
                hints.push((display_key(&km.services).into(), "services"));
            }
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
            // A clean run walks back to the dashboard by itself after a
            // moment; "now" is the difference between the key being the way
            // out and the key being a shortcut past the pause.
            Some(BatchPhase::Done) => {
                let leaving = state.batch.as_ref().is_some_and(|b| b.returns_on_its_own());
                vec![(
                    display_key(&km.back).into(),
                    if leaving { "back to repos now" } else { "back to repos" },
                )]
            }
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
        // Hook output on screen: the pane writes its own keys along its own
        // bottom border — scrolling, the error walk, yanking, the way out —
        // so the footer carries what is left of the view behind it rather
        // than saying the same six things two rows lower.
        View::GitStatus if state.current_op().is_some() => {
            let running = state.current_op().is_some_and(|o| !o.finished);
            let mut hints = vec![
                (
                    format!("{}/{}", display_key(&km.scroll_top), display_key(&km.scroll_bottom)),
                    "top/tail",
                ),
            ];
            // Refused while a git command is in flight, so not offered either.
            if !running {
                hints.push((display_key(&km.git_push).into(), "push"));
            }
            hints.push((display_key(&km.refresh).into(), "refresh"));
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
            hints.push((display_key(&km.refresh).into(), "refresh"));
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

    // For a new session's first seconds, `? help` breathes in the accent: the
    // one hint a first run needs, announced without a banner. The breath
    // shallows as the session ages and retires for good the moment help is
    // opened — an invitation to a place you have been is just blinking.
    let beacon = if !state.help_seen && state.tick_count < HELP_BEACON_TICKS {
        let m = Motion::new(state.tick_count);
        m.pulse(14) * (1.0 - state.tick_count as f64 / HELP_BEACON_TICKS as f64)
    } else {
        0.0
    };

    let last = hints.len() - 1;
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        let (key_style, desc_style) = if i == last && beacon > 0.0 {
            (
                Style::default().fg(mix(theme.text_bright, theme.accent, beacon)).bold(),
                Style::default().fg(mix(theme.text_muted, theme.accent, beacon)),
            )
        } else {
            (
                Style::default().fg(theme.text_bright).bold(),
                Style::default().fg(theme.text_muted),
            )
        };
        spans.push(Span::styled(key.clone(), key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, desc_style));
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
        let color = match state.status_kind {
            StatusKind::Error => theme.failure,
            StatusKind::Success => theme.success,
            StatusKind::Info => theme.text_bright,
        };
        spans.push(Span::styled(msg.clone(), Style::default().fg(color)));
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

/// Register the visible rows of a just-drawn table as click targets.
///
/// Replicates ratatui's ensure-visible scroll for a `TableState` built fresh
/// each frame: offset starts at 0 and moves only as far as it must for the
/// selected row to fit. Uniform `row_h` per table is an invariant of the
/// call sites, not of ratatui. A table that never scrolls (rendered without
/// state) passes `selected_row = 0`, which pins the offset to the top.
fn register_table_hits(
    state: &AppState,
    inner: Rect,
    header_rows: u16,
    row_h: u16,
    total_rows: usize,
    selected_row: usize,
    hit_for: impl Fn(usize) -> Option<Hit>,
) {
    let avail = inner.height.saturating_sub(header_rows);
    if avail == 0 || row_h == 0 || total_rows == 0 {
        return;
    }
    let visible = ((avail / row_h) as usize).max(1);
    let offset = (selected_row + 1).saturating_sub(visible);
    let mut hits = state.hits.borrow_mut();
    for r in offset..total_rows.min(offset + visible) {
        let Some(h) = hit_for(r) else { continue };
        let y = inner.y + header_rows + ((r - offset) as u16) * row_h;
        hits.push((
            Rect { x: inner.x, y, width: inner.width, height: row_h },
            h,
        ));
    }
}

fn render_repos(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;

    // On a wide terminal, anything mid-flight earns a live pane down the right
    // edge: the dashboard stays the view, and the log of the run you most
    // recently set moving scrolls beside it — the wall-monitor arrangement.
    let (area, live_pane) = if area.width >= super::DASH_SPLIT_MIN_WIDTH
        && state.dash_tail_target().is_some()
    {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(56)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };
    if let Some(pa) = live_pane {
        render_dash_live(f, pa, state);
    }

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

    // Runtime health next to CI health, when Uptime Kuma is configured and
    // any monitor mapped itself to a row. All-or-nothing like the other
    // optional columns: on a narrow terminal the run itself matters more.
    let show_live = inner.width >= 100
        && state
            .repos
            .iter()
            .any(|c| state.repo_services(&c.spec).next().is_some());

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
    if show_live {
        header_cells.insert(5, Cell::from(Span::styled("Live", hdr)));
    }
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

    // What the repo's monitors say it is doing in production right now.
    // Worst news wins the cell: one down service among three is a down cell,
    // because "mostly up" is not a state anyone acts on.
    let live_cell = |card: &crate::app::state::RepoCard| -> Cell<'static> {
        use crate::kuma::ServiceState;
        let svcs: Vec<_> = state.repo_services(&card.spec).collect();
        if svcs.is_empty() {
            return Cell::from("");
        }
        let down = svcs.iter().filter(|s| s.state == ServiceState::Down).count();
        if down > 0 {
            let label = if svcs.len() == 1 {
                // The day's uptime says whether this is a blip or a siege.
                match svcs[0].uptime24 {
                    Some(u) => format!("✗ down · {:.0}%", u * 100.0),
                    None => "✗ down".to_string(),
                }
            } else {
                format!("✗ {down}/{} down", svcs.len())
            };
            return Cell::from(Span::styled(label, Style::default().fg(theme.failure).bold()));
        }
        if svcs.iter().all(|s| s.state != ServiceState::Up) {
            return Cell::from(Span::styled(
                "◌ pending",
                Style::default().fg(theme.text_muted).italic(),
            ));
        }
        // Up: the slowest answer is the honest one number to show for the row.
        let label = match svcs.iter().filter_map(|s| s.ping_ms).max() {
            Some(p) => format!("● {p}ms"),
            None => "● up".to_string(),
        };
        Cell::from(Span::styled(label, Style::default().fg(theme.success)))
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
                let mut cells = vec![
                    mark_cell(card),
                    Cell::from(Span::styled("!", Style::default().fg(theme.failure).bold())),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                ];
                if show_live {
                    cells.push(live_cell(card));
                }
                cells.push(Cell::from(Span::styled(
                    truncate(&err.text, 46),
                    Style::default().fg(theme.failure),
                )));
                cells.extend(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")));
                rows.push(Row::new(cells)
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
                let mut cells = vec![
                    mark_cell(card),
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                ];
                if show_live {
                    cells.push(live_cell(card));
                }
                cells.push(Cell::from(Span::styled(
                    "local only — no GitHub remote",
                    Style::default().fg(theme.text_faint).italic(),
                )));
                cells.extend(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")));
                rows.push(Row::new(cells)
                .style(row_dress(
                    theme.row_idle,
                    rows.len(),
                    i == state.repo_cursor,
                    theme,
                )));
                continue;
            }

            if !card.loaded {
                let mut cells = vec![
                    mark_cell(card),
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                ];
                if show_live {
                    cells.push(live_cell(card));
                }
                cells.push(Cell::from(Line::from(skeleton(14, state.tick_count, theme))));
                cells.extend(cols.tail(Cell::from(""), Cell::from(""), Cell::from("")));
                rows.push(Row::new(cells)
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
            // against your memory of it. A *landing* outranks a mere change:
            // it breathes in the verdict's colour for two full breaths —
            // longer and warmer than the flash — because "it finished, and
            // this is how" is the fact the dashboard exists to deliver.
            let m = Motion::new(state.tick_count);
            let base = row_bg_for_status(status, theme);
            let settle = card
                .settled_tick
                .map(|(at, verdict)| (m.decay(at, SETTLE_TICKS), verdict))
                .filter(|(envelope, _)| *envelope > 0.0);
            let bg = if let Some((envelope, verdict)) = settle {
                let tint = style_for_status(verdict, theme).fg.unwrap_or(theme.text);
                // The pulse carries the breath; the decay fades it out.
                mix(base, tint, envelope * (0.2 + 0.35 * m.pulse(12)))
            } else if let Some(at) = card.changed_tick {
                mix(
                    base,
                    style_for_status(status, theme).fg.unwrap_or(theme.text),
                    0.4 * m.decay(at, FLASH_TICKS),
                )
            } else {
                base
            };
            let dress = row_dress(bg, rows.len(), i == state.repo_cursor, theme);

            let mut cells = vec![
                mark_cell(card),
                Cell::from(Span::styled(
                    animated_glyph(status, state.tick_count),
                    style_for_status(status, theme),
                )),
                name_cell,
                local_cell(card),
                changes_cell(card),
            ];
            if show_live {
                cells.push(live_cell(card));
            }
            cells.push(Cell::from(Span::styled(workflow, Style::default().fg(theme.text))));
            cells.extend(cols.tail(
                ran_on_cell(card, &branch),
                // Right-aligned: "4d ago" over "130d ago" with ragged units is
                // the most reliable tell that a table was never looked at.
                Cell::from(Line::from(Span::styled(when_text, when_style)).right_aligned()),
                Cell::from(sparkline),
            ));
            Row::new(cells).style(dress)
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
    if show_live {
        widths.insert(5, Constraint::Length(12)); // service health
    }
    if cols.ran_on {
        widths.push(Constraint::Fill(26)); // branch the run used
    }
    if cols.updated {
        widths.push(Constraint::Length(10));
    }
    if cols.recent {
        widths.push(Constraint::Length(spark_w as u16 + 9));
    }

    let rows_len = rows.len();
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

    // The entrance: the table rises out of the dashboard's own ground one line
    // at a time, top to bottom — the sweep the help and services cards play,
    // on the view you actually land on rather than only on the ones you open.
    //
    // Done over the drawn buffer rather than over the rows: a built `Row` will
    // not hand its cells back to be recoloured, and what sweeps here is where a
    // line sits on screen, not which repo holds it. Only the lines the table
    // really drew are touched — blending the empty space below the last row
    // would paint a slab of ground there and then take it away again.
    if state.dash_opened_tick.is_some() {
        let body = inner.height.saturating_sub(HEADER_LINES) as usize;
        let drawn = HEADER_LINES as usize + rows_len.saturating_sub(ts.offset()).min(body);
        let ground = theme.row_idle;
        let buf = f.buffer_mut();
        for line in 0..drawn.min(inner.height as usize) {
            let p = dash_reveal_at(state, line);
            if p >= 1.0 {
                continue;
            }
            let y = inner.y + line as u16;
            for x in inner.x..inner.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    // `Reset` is the terminal's own colour, which cannot be
                    // blended and would snap to the card's ground — a slab of
                    // paint appearing under the column heads and then leaving.
                    // Those cells simply sit the sweep out.
                    if cell.fg != Color::Reset {
                        cell.fg = mix(ground, cell.fg, p);
                    }
                    if cell.bg != Color::Reset {
                        cell.bg = mix(ground, cell.bg, p);
                    }
                }
            }
        }
    }

    // Click targets, through the same heading-aware map. Headings themselves
    // register nothing — there is nothing selecting one could mean.
    let total_rows = rows_len;
    let mut repo_of_row: Vec<Option<usize>> = vec![None; total_rows];
    for (repo, &row) in row_of.iter().enumerate() {
        repo_of_row[row] = Some(repo);
    }
    register_table_hits(
        state,
        inner,
        2,
        1,
        total_rows,
        row_of.get(state.repo_cursor).copied().unwrap_or(0),
        |r| repo_of_row.get(r).copied().flatten().map(Hit::Repo),
    );
}

/// The live pane's content when there is no log body to show: the running
/// job's own step list, newest at the bottom.
///
/// GitHub's REST log endpoint serves the archive, and a job's archive does
/// not exist until the job ends — so for a run still in flight there is no
/// tail to serve, and "waiting for GitHub" was a wait that never ended. The
/// steps are the part that genuinely moves: the poll already carries them,
/// with their own clocks, and they tick over every few seconds.
fn live_step_lines(job: &Job, rows: usize, tick: u64, theme: &Theme) -> Vec<Line<'static>> {
    if rows == 0 || job.steps.is_empty() {
        return Vec::new();
    }
    // Anchored on the step in flight, showing the ones behind it: the same
    // "newest at the bottom" reading a log tail has, so the eye lands in the
    // same place whichever of the two the pane happens to be showing.
    let end = job
        .steps
        .iter()
        .position(|s| !s.status.is_terminal())
        .map(|i| i + 1)
        .unwrap_or(job.steps.len());
    let start = end.saturating_sub(rows);
    job.steps[start..end]
        .iter()
        .map(|s| {
            let secs = s.started_at.map(|st| {
                (s.completed_at.unwrap_or_else(Utc::now) - st).num_seconds().max(0)
            });
            let dur = secs.map(format_elapsed).unwrap_or_default();
            Line::from(vec![
                Span::styled(
                    animated_glyph(s.status, tick),
                    style_for_status(s.status, theme),
                ),
                Span::raw(" "),
                Span::styled(
                    s.name.clone(),
                    if s.status == Status::Running {
                        Style::default().fg(theme.text_bright)
                    } else {
                        Style::default().fg(theme.text)
                    },
                ),
                Span::styled(
                    if dur.is_empty() { String::new() } else { format!("  {dur}") },
                    Style::default().fg(theme.text_muted),
                ),
            ])
        })
        .collect()
}

/// The dashboard's live pane: which run is being followed, where it stands,
/// and the tail of its running job's log — the Watch view's pane, moved to
/// where the eye already is when several repos are being minded at once.
fn render_dash_live(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let Some((spec, detail, job)) = state.dash_tail_target() else {
        return;
    };
    let blk = styled_block(&format!("Live — {spec}"), theme);
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    if inner.height < 4 {
        return;
    }

    let step = detail.current_step().unwrap_or("—");
    let head = vec![
        Line::from(vec![
            Span::styled(
                animated_glyph(detail.run.status, state.tick_count),
                style_for_status(detail.run.status, theme),
            ),
            Span::raw(" "),
            Span::styled(
                detail.run.display_title.clone(),
                Style::default().fg(theme.text_bright).bold(),
            ),
            Span::styled(
                format!("  ({})", detail.run.head_branch),
                Style::default().fg(theme.text_muted),
            ),
        ]),
        Line::from(vec![
            Span::styled("step ", Style::default().fg(theme.text_faint)),
            Span::styled(step.to_string(), Style::default().fg(theme.text)),
            Span::styled(
                format!("  ·  {}", format_elapsed(elapsed_seconds(&detail.run))),
                Style::default().fg(theme.text_muted),
            ),
        ]),
        Line::default(),
    ];
    let tail_rows = inner.height.saturating_sub(head.len() as u16) as usize;
    let mut lines = head;
    match &state.watch_tail {
        Some(t) if t.job_id == job.id && !t.lines.is_empty() => {
            // The tail follows the newest lines by definition; history is what
            // entering the run is for.
            lines.extend(t.lines.iter().rev().take(tail_rows).rev().map(|l| {
                Line::from(ansi_line_to_spans(l, Style::default().fg(theme.text)))
            }));
        }
        // No log body: show the steps instead, which are live where the log
        // cannot be until the job has finished.
        _ => match live_step_lines(job, tail_rows, state.tick_count, theme) {
            steps if steps.is_empty() => lines.push(Line::from(Span::styled(
                "waiting for the first step…",
                Style::default().fg(theme.text_muted).italic(),
            ))),
            steps => lines.extend(steps),
        },
    }
    f.render_widget(Paragraph::new(lines), inner);
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
///
/// `pub(super)` along with the two below: the event loop reads them to know
/// whether a flash is still animating and the 10fps redraw still earned.
pub(super) const FLASH_TICKS: u64 = 10;

/// How long a row breathes in its verdict colour after a run lands — two full
/// breaths. Longer than the any-change flash because a landing is the moment
/// the dashboard exists for.
pub(super) const SETTLE_TICKS: u64 = 24;

/// How long the all-green sweep takes to cross the header and fade.
pub(super) const CELEBRATE_TICKS: u64 = 20;

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
                // What the row's own recent runs say is left of this one —
                // the question every glance at the strip is actually asking.
                eta: typical_run_secs(card.runs.iter(), &run.display_title)
                    .map(|t| eta_text(t, elapsed_seconds(run)))
                    .unwrap_or_default(),
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
    // The ETA column exists only when some row has one to show and the strip
    // is wide enough that its two-space toll doesn't come out of the step
    // name; without it the layout is exactly what it was before ETAs existed.
    let w_eta = longest(&|c| disp_width(&c.eta)).min(14);
    let show_eta = w_eta > 0 && inner >= 80;
    let (w_eta, eta_spacing) = if show_eta { (w_eta, SPACING) } else { (0, 0) };
    let fixed = 1 + bar_w + w_count + w_elapsed + w_eta + eta_spacing + SPACING * 8;
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
            let mut cells_out = vec![
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
            ];
            if show_eta {
                cells_out.push(Cell::from(
                    Line::from(Span::styled(
                        c.eta.clone(),
                        Style::default().fg(theme.text_muted),
                    ))
                    .right_aligned(),
                ));
            }
            cells_out.push(Cell::from(
                Line::from(Span::styled(
                    c.elapsed.clone(),
                    Style::default().fg(theme.warning),
                ))
                .right_aligned(),
            ));
            Row::new(cells_out)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1), // spinner
        Constraint::Length(cols[0] as u16),
        Constraint::Length(cols[1] as u16),
        Constraint::Length(cols[2] as u16),
        Constraint::Length(cols[3] as u16),
        Constraint::Length(cols[4] as u16),
        Constraint::Length(bar_w as u16),
        Constraint::Length(w_count as u16),
    ];
    if show_eta {
        widths.push(Constraint::Length(w_eta as u16));
    }
    widths.push(Constraint::Length(w_elapsed as u16));

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
    /// "~1:16 left", from this workflow's own recent durations. Empty when
    /// there is no history to predict from.
    eta: String,
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
                (format!("{}/⌫", k(&km.back)), "back one view"),
                (k(&km.finder), "fuzzy find in the current list"),
                (k(&km.refresh), "re-fetch whatever this screen shows"),
                (k(&km.repos_view), "multi-repo dashboard"),
                (k(&km.services), "service health, by monitor name"),
                (k(&km.snooze), "snooze notifications — 30m, 60m, off"),
                (k(&km.open_browser), "open in browser"),
                (k(&km.yank), "copy the selection to the clipboard"),
                ("click".into(), "select a row — again to open; wheel scrolls"),
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
                (format!("{}/↵", k(&km.git_diff)), "diff every changed file, from this one"),
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
                ("layout".into(), "side by side — unified when the terminal is narrow"),
                (pair(&km.down, &km.up), "scroll"),
                (pair(&km.page_down, &km.page_up), "page"),
                (pair(&km.scroll_top, &km.scroll_bottom), "top / bottom"),
                (pair(&km.next_step, &km.prev_step), "next / previous changed file"),
                (k(&km.git_stage), "stage / unstage this file"),
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

/// Does one help row survive the card's filter? Every whitespace-separated
/// term has to turn up somewhere in the binding or its description, so
/// "push branch" narrows where either word alone would not. Substring, not
/// fuzzy: on prose this short a subsequence match keeps almost every row,
/// which is the one thing a filter must not do.
fn help_row_matches(key: &str, desc: &str, query: &str) -> bool {
    let hay = format!("{key} {desc}").to_lowercase();
    query
        .split_whitespace()
        .all(|term| hay.contains(&term.to_lowercase()))
}

/// `s` cut into spans with every occurrence of a search term lifted into
/// `hit` — the row shows *why* it survived, rather than leaving the eye to
/// re-run the search the card just ran.
fn highlight_terms(s: &str, query: &str, base: Style, hit: Style) -> Vec<Span<'static>> {
    // One lowercase char per source char, so a position in the fold is a
    // position in the original — `flat_map` over `to_lowercase` would not
    // hold that, and the spans would slice at the wrong place.
    let lower: Vec<char> = s
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let src: Vec<char> = s.chars().collect();
    let mut mark = vec![false; src.len()];
    for term in query.split_whitespace() {
        let t: Vec<char> = term
            .chars()
            .map(|c| c.to_lowercase().next().unwrap_or(c))
            .collect();
        if t.is_empty() || t.len() > lower.len() {
            continue;
        }
        for i in 0..=(lower.len() - t.len()) {
            if lower[i..i + t.len()] == t[..] {
                mark[i..i + t.len()].iter_mut().for_each(|m| *m = true);
            }
        }
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let on = mark[i];
        let start = i;
        while i < src.len() && mark[i] == on {
            i += 1;
        }
        let text: String = src[start..i].iter().collect();
        out.push(Span::styled(text, if on { hit } else { base }));
    }
    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
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
    // The filter, applied before anything is measured: the key column and the
    // two-column fold are both sized from what actually survives.
    let query = state.help_search.trim().to_string();
    if !query.is_empty() {
        for (_, rows) in sections.iter_mut() {
            rows.retain(|(k, d)| help_row_matches(k, d, &query));
        }
        sections.retain(|(_, rows)| !rows.is_empty());
    }
    // Float the current view's section to just below Global, so "what can I do
    // here?" is answered from the top-left corner, before any reading order.
    // Below Global, that is — unless a filter has taken Global away, in which
    // case there is nothing left to float under and the section goes to the top.
    if let Some(i) = sections.iter().position(|(t, _)| *t == current) {
        let dest = usize::from(sections.first().is_some_and(|(t, _)| *t == "Global"));
        if i > dest {
            let s = sections.remove(i);
            sections.insert(dest, s);
        }
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
            // up the accent so the eye finds its keys first. Cut from the
            // darker overlay tone, now that the card itself stands on
            // `surface_alt` — a cap the colour of its card is no cap at all.
            let cap_style = Style::default()
                .bg(theme.overlay)
                .fg(if is_current { theme.accent } else { theme.text_bright })
                .bold();
            let base = Style::default().fg(theme.text);
            let hit = Style::default().fg(theme.accent).bold();
            for (i, seg) in wrap_words(desc, desc_w).into_iter().enumerate() {
                let mut line = if i == 0 {
                    vec![
                        Span::raw(" "),
                        Span::styled(format!(" {key:>key_w$} "), cap_style),
                        Span::raw("  "),
                    ]
                } else {
                    vec![Span::raw(" ".repeat(key_w + 5))]
                };
                if query.is_empty() {
                    line.push(Span::styled(seg, base));
                } else {
                    line.extend(highlight_terms(&seg, &query, base, hit));
                }
                out.push(Line::from(line));
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
    // A filter that matches nothing says so, rather than leaving an empty
    // frame that looks like the card failed to draw.
    if left.is_empty() && right.is_empty() {
        left.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("no binding matches “{query}”"),
                Style::default().fg(theme.text_faint).italic(),
            ),
        ]));
    }

    let content_h = left.len().max(right.len()) as u16;
    // + 2 borders + 2 rows of breathing room, like the services card: caps
    // touching the frame read as part of it rather than as keys on a card.
    let dialog_h = (content_h + 4).min(area.height.saturating_sub(2)).max(8);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let inner_h = dialog_h.saturating_sub(4);
    let max_scroll = content_h.saturating_sub(inner_h);
    // What the scroll keys and the wheel clamp against.
    state.last_help_max_scroll.set(max_scroll);
    let scroll = state.help_scroll.min(max_scroll);

    // The bottom rail doubles as the search field: while typing, it is the
    // query and a cursor, and nothing else — a card that keeps offering
    // "any key closes" while every key is going into a text field is lying.
    let find = display_key(&state.keymap.search);
    let footer = if state.help_typing {
        format!(" {find} {}▏ · ↵ keeps it · Esc clears ", state.help_search)
    } else if !query.is_empty() {
        format!(" filtered: {query} · {find} edits · Esc clears ")
    } else if max_scroll > 0 {
        format!(
            " {}/{} or wheel scroll · {}–{} of {} · {find} search · any other key closes ",
            display_key(&state.keymap.down),
            display_key(&state.keymap.up),
            scroll + 1,
            (scroll + inner_h).min(content_h),
            content_h,
        )
    } else {
        format!(" {find} search · any other key closes ")
    };
    let footer_style = if state.help_typing {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.text_faint)
    };

    // The same dress as the services card: a chip cut into an accent frame,
    // floating on its own solid ground — Clear alone leaves default cells,
    // which a translucent terminal renders as wallpaper behind the text.
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled("─┤ ", Style::default().fg(theme.accent)),
            Span::styled(
                format!("? Help — jog v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme.accent).bold(),
            ),
            Span::styled(" ├", Style::default().fg(theme.accent)),
        ]))
        .title_alignment(ratatui::layout::Alignment::Center)
        .title_bottom(
            Line::from(Span::styled(footer, footer_style)).centered(),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .padding(Padding::vertical(1))
        .style(Style::default().bg(theme.surface_alt));

    // The entrance: rows rise out of the card's own ground in one sweep down
    // the card, both columns abreast — colour and cap backgrounds arrive
    // together, so the card's shape never jumps. Indexed by content row, not
    // screen row: rows scrolled into view mid-entrance simply join the sweep.
    let ground = theme.surface_alt;
    let fade_rows = |rows: Vec<Line<'static>>| -> Vec<Line<'static>> {
        rows.into_iter()
            .enumerate()
            .map(|(i, line)| {
                let p = help_reveal_at(state, i);
                if p >= 1.0 {
                    return line;
                }
                let spans: Vec<Span<'static>> = line
                    .spans
                    .into_iter()
                    .map(|mut s| {
                        if let Some(fg) = s.style.fg {
                            s.style.fg = Some(mix(ground, fg, p));
                        }
                        if let Some(bg) = s.style.bg {
                            s.style.bg = Some(mix(ground, bg, p));
                        }
                        s
                    })
                    .collect();
                Line::from(spans)
            })
            .collect()
    };
    let (left, right) = (fade_rows(left), fade_rows(right));

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
    register_table_hits(
        state,
        chunks[1],
        2,
        1,
        gv.entries().len(),
        gv.cursor,
        |r| Some(Hit::GitEntry(r)),
    );
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

/// The keys the hook-output pane answers to, in the order they are worth
/// knowing. Scrolling and yanking always; the error walk only when there are
/// errors to walk; and then the way out — which in the Changes view is also
/// the way forward, because a failed hook leaves a commit to fix and make
/// again. A batch keeps its own retry/skip rail across the queue above.
fn op_key_rail(op: &GitOp, state: &AppState) -> Vec<(String, &'static str)> {
    let km = &state.keymap;
    let pair = |a: &str, b: &str| format!("{}/{}", display_key(a), display_key(b));
    let mut rail = vec![(pair(&km.down, &km.up), "scroll")];
    if op.error_count() > 0 {
        rail.push((pair(&km.next_error, &km.prev_error), "error"));
    }
    rail.push((display_key(&km.yank).to_string(), "yank"));
    if !op.finished {
        // The command is still going: `back` steps out of the view and leaves
        // it running rather than killing it, so say that and not "dismiss".
        rail.push((display_key(&km.back).to_string(), "leave, keeps running"));
        return rail;
    }
    // In a batch, retry/skip/stop belong to the queue and are already written
    // across the top of it — repeating them here would say the same thing
    // twice and crowd out the keys that are only on this pane.
    if state.view != View::BatchCommit {
        rail.push((display_key(&km.back).to_string(), "dismiss"));
        // A hook that failed usually left something to fix — and a formatter
        // that rewrote the files leaves them unstaged, which is the one step
        // people forget before committing again.
        if op.failed {
            rail.push((display_key(&km.git_stage_all).to_string(), "stage all"));
            rail.push((display_key(&km.git_commit).to_string(), "commit again"));
        }
    }
    rail
}

/// A key rail as one line, cut to `width` by dropping whole entries off the
/// end — a truncated rail is a rail whose last key reads as something it is
/// not, and the app footer still carries the full set.
fn rail_line(rail: &[(String, &'static str)], width: u16, theme: &Theme) -> Line<'static> {
    // Two corners, and a space either side of the run so it never touches them.
    let room = width.saturating_sub(4) as usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (key, desc) in rail {
        let sep = if spans.is_empty() { "" } else { " · " };
        let cost = disp_width(sep) + disp_width(key) + 1 + disp_width(desc);
        if used + cost > room {
            break;
        }
        used += cost;
        if !sep.is_empty() {
            spans.push(Span::styled(sep, Style::default().fg(theme.border_dim)));
        }
        spans.push(Span::styled(
            key.clone(),
            Style::default().fg(theme.text_bright).bold(),
        ));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(theme.text_faint),
        ));
    }
    if spans.is_empty() {
        return Line::default();
    }
    spans.insert(0, Span::raw(" "));
    spans.push(Span::raw(" "));
    Line::from(spans)
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

    // What can be done *with this pane*, written on the pane. When a hook says
    // no, the keys that get you back out — read the failure, copy it, fix the
    // file, try again — are worth more on the box being stared at than on the
    // app footer at the bottom of the screen, and a failed commit is exactly
    // the moment nobody wants to go looking for `?`.
    let rail = op_key_rail(op, state);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(border).bold(),
        ))
        .title_bottom(rail_line(&rail, area.width, theme).right_aligned());
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

/// The narrowest a column of diff text can be and still be worth reading. Below
/// it the two columns are giving each other's width away to gutters and a rule,
/// and one wide column of unified diff says more.
const DIFF_SIDE_MIN: usize = 22;

/// Where the two columns fall inside `width`: the line-number gutter each side
/// gets, and the text width each side gets. `None` when the terminal cannot
/// spare them — the unified layout is the honest answer on a narrow screen
/// rather than two columns of ellipses.
fn diff_columns(width: usize, rows: &[DiffRow]) -> Option<(usize, usize, usize)> {
    let widest = rows
        .iter()
        .filter_map(|r| match r {
            DiffRow::Pair { old, new } => Some(
                old.as_ref()
                    .and_then(|s| s.num)
                    .max(new.as_ref().and_then(|s| s.num))
                    .unwrap_or(0),
            ),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    // Room for the number itself, never less than two columns so the gutter
    // does not visibly shift as a diff scrolls from line 9 to line 10.
    let gutter = widest.to_string().len().clamp(2, 6);
    // Two gutters, a space after each, and the rule between the columns.
    let text = width.checked_sub(2 * (gutter + 1) + DIFF_RULE.len())?;
    // An odd column of width goes to the new side rather than being left
    // unpainted at the edge: it is the half being read.
    let (left, right) = (text / 2, text - text / 2);
    (left >= DIFF_SIDE_MIN).then_some((gutter, left, right))
}

/// The rule between the two columns.
const DIFF_RULE: &str = " │ ";

/// How far a changed line's ground is carried toward its own colour, and how
/// far again under the span that actually differs. Two tones of one hue rather
/// than a tint and an inversion: the deeper block says "here" without the row
/// having to change what colour its text is.
///
/// `DIFF_SPAN_LIFT` is how far that span's text is lifted toward white, which
/// is what keeps it legible on the deeper ground. On a 256-colour terminal
/// none of these can be blended — the row snaps to its own colour and the
/// span's mark comes down to the bold, which is the honest degradation.
const DIFF_ROW_TINT: f64 = 0.16;
const DIFF_SPAN_TINT: f64 = 0.38;
const DIFF_SPAN_LIFT: f64 = 0.45;

/// How long a file band's entrance runs, in ticks at the app's 100ms clock.
/// Long enough that the sweep is watched rather than glimpsed: at 100 columns
/// the edge crosses about eight columns a frame, a motion the eye can follow.
const BAND_WIPE_TICKS: f64 = 12.0;

/// How far the band for file `fi` is into its entrance, 0.0 → 1.0 — marking
/// the tick it was first drawn on the way through.
///
/// Per band, not per view: a band plays its sweep when it first scrolls onto
/// the screen, so the tenth file's divider arrives when the reader does, not
/// invisibly at open. 1.0 when the view has no opening tick (a test, a direct
/// construction): a settled band is the honest answer there, the same rule
/// every entrance in this file follows.
fn band_wipe_at(dv: &GitDiffView, fi: usize, tick: u64) -> f64 {
    if dv.opened_tick.is_none() {
        return 1.0;
    }
    let mut seen = dv.band_seen.borrow_mut();
    let Some(slot) = seen.get_mut(fi) else {
        return 1.0;
    };
    let since = tick.saturating_sub(*slot.get_or_insert(tick)) as f64 + 1.0;
    (since / BAND_WIPE_TICKS).clamp(0.0, 1.0)
}

/// Whether any band on the open diff is still mid-sweep — the redraw loop's
/// question. Bands not yet seen don't count: they cost nothing until the
/// scroll that reveals them, and that keypress redraws on its own.
pub fn diff_bands_revealing(state: &AppState) -> bool {
    matches!(state.view, View::GitDiff)
        && state.git_diff.as_ref().is_some_and(|dv| {
            dv.opened_tick.is_some()
                && dv.band_seen.borrow().iter().flatten().any(|&t| {
                    (state.tick_count.saturating_sub(t) as f64) < BAND_WIPE_TICKS
                })
        })
}

/// The Nerd Font mark for a file's language, by extension — the set the
/// devicon-styled tools already made familiar, so a Rust gear or a Python
/// snake on a band reads at a glance. Anything unrecognised gets a plain
/// file, which is the honest mark for it; the same tradeoff and override
/// story as the forge mark applies (`ui.file_icons`).
fn lang_icon(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.eq_ignore_ascii_case("dockerfile") {
        return "\u{f308}"; // docker whale
    }
    if name.eq_ignore_ascii_case("makefile") {
        return "\u{f489}"; // terminal
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "\u{e7a8}",                          // rust gear
        "py" => "\u{e73c}",                          // python
        "ts" | "tsx" => "\u{e628}",                  // typescript
        "js" | "jsx" | "mjs" | "cjs" => "\u{e74e}",  // javascript
        "go" => "\u{e627}",                          // gopher
        "java" => "\u{e738}",
        "c" | "h" => "\u{e61e}",
        "cpp" | "cc" | "cxx" | "hpp" => "\u{e61d}",
        "rb" => "\u{e739}",                          // ruby
        "php" => "\u{e73d}",
        "lua" => "\u{e620}",
        "swift" => "\u{e755}",
        "kt" | "kts" => "\u{e634}",                  // kotlin
        "html" | "htm" => "\u{e736}",
        "css" | "scss" | "sass" | "less" => "\u{e749}",
        "json" => "\u{e60b}",
        "md" | "markdown" => "\u{e73e}",
        "yml" | "yaml" | "toml" | "ini" | "conf" => "\u{e615}", // config cog
        "sh" | "bash" | "zsh" | "fish" => "\u{f489}",           // terminal
        "lock" => "\u{f023}",                                   // padlock
        _ => "\u{f016}",                                        // plain file
    }
}

/// The band naming one file: a language mark, the filename and its own
/// +/− counts on a solid run of the accent colour, the
/// full width of the pane — a chapter head no row of diff text can be
/// mistaken for, so where one file ends and the next begins is a glance, not
/// a search.
///
/// `wipe` is how far the band is into its entrance: below 1.0 only that share
/// of it has been painted, so the colour eats its way from the left edge to
/// the right — the name and counts appearing as the sweep passes over them,
/// a comet tail of fading blocks riding its leading edge so the motion is a
/// thing on screen, not just ground appearing.
fn file_banner(
    path: &str,
    add: usize,
    del: usize,
    width: usize,
    wipe: f64,
    icon: &str,
    theme: &Theme,
) -> Line<'static> {
    // The same ground the help view sets its key caps on: accent under
    // surface-coloured text, the highest-contrast pairing the theme owns.
    let band = Style::default().bg(theme.accent).fg(theme.surface);
    let lead = if icon.is_empty() {
        " ".to_string()
    } else {
        format!(" {icon} ")
    };
    // The counts carry their own meaning: what the file gained in green, what
    // it lost in red. Both are taken down towards `surface` first — the raw
    // success/failure colours are pitched for a dark background and would sit
    // on the light band with nearly no contrast at all.
    let added = band.fg(mix(theme.success, theme.surface, 0.55)).bold();
    let removed = band.fg(mix(theme.failure, theme.surface, 0.55)).bold();
    let plus = format!("  +{add}");
    let minus = format!(" −{del}");
    let stats_w = disp_width(&plus) + disp_width(&minus);
    let path = truncate(
        path,
        width.saturating_sub(disp_width(&lead) + 1 + stats_w).max(1),
    );
    let pad = width.saturating_sub(disp_width(&lead) + disp_width(&path) + stats_w);
    if wipe >= 1.0 {
        return Line::from(vec![
            Span::styled(format!("{lead}{path}"), band.bold()),
            Span::styled(plus, added),
            Span::styled(minus, removed),
            Span::styled(" ".repeat(pad), band),
        ]);
    }
    // Mid-sweep. Linear on purpose: an eased edge spends most of its life
    // nearly done, and the whole point here is watching it travel.
    let keep = (width as f64 * wipe).round() as usize;
    let (head, head_w) = fit_columns(&format!("{lead}{path}"), keep);
    let (plus, plus_w) = fit_columns(&plus, keep.saturating_sub(head_w));
    let (minus, minus_w) = fit_columns(&minus, keep.saturating_sub(head_w + plus_w));
    let fill = keep.saturating_sub(head_w + plus_w + minus_w);
    // The comet tail ahead of the solid ground, densest where it leaves it.
    let edge: String = ["▓", "▒", "░"]
        .iter()
        .take(width.saturating_sub(keep))
        .copied()
        .collect();
    Line::from(vec![
        Span::styled(head, band.bold()),
        Span::styled(plus, added),
        Span::styled(minus, removed),
        Span::styled(" ".repeat(fill), band),
        Span::styled(edge, Style::default().fg(theme.accent)),
    ])
}

/// Longest prefix of `s` that fits in `cols` terminal columns, and the width
/// it actually takes. `truncate` is the wrong tool mid-sweep: its ellipsis
/// would put a flickering `…` on the band's leading edge every frame.
fn fit_columns(s: &str, cols: usize) -> (String, usize) {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > cols {
            break;
        }
        out.push(c);
        w += cw;
    }
    (out, w)
}

/// One side-by-side row as a drawn line. `wipe` is the file band's entrance
/// progress and `icon` its language mark; every other kind of row ignores
/// both.
fn diff_row_line(
    row: &DiffRow,
    gutter: usize,
    left: usize,
    right: usize,
    wipe: f64,
    icon: &str,
    theme: &Theme,
) -> Line<'static> {
    let width = 2 * (gutter + 1) + DIFF_RULE.len() + left + right;
    match row {
        DiffRow::File { path, add, del } => {
            file_banner(path, *add, *del, width, wipe, icon, theme)
        }
        DiffRow::Section(label) => Line::from(Span::styled(
            truncate(&format!("── {label} "), width),
            Style::default().fg(theme.accent).bold(),
        )),
        DiffRow::Meta(t) => Line::from(Span::styled(
            truncate(t, width),
            diff_line_style(t, theme),
        )),
        DiffRow::Pair { old, new } => {
            // The gap opposite an added or removed line: a ground of its own,
            // because nothing was there — neither the file's ordinary ground
            // nor a change.
            let gap = mix(theme.surface, theme.overlay, 0.5);

            let cell = |s: Option<&DiffSide>, side: usize, tint: Color| -> Vec<Span<'static>> {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
                let Some(s) = s else {
                    spans.push(Span::styled(
                        " ".repeat(gutter + 1 + side),
                        Style::default().bg(gap),
                    ));
                    return spans;
                };
                let bg = if s.changed {
                    mix(theme.surface, tint, DIFF_ROW_TINT)
                } else {
                    theme.surface
                };
                let fg = if s.changed { tint } else { theme.text };
                let num = match s.num {
                    Some(n) => format!("{n:>gutter$} "),
                    None => " ".repeat(gutter + 1),
                };
                spans.push(Span::styled(
                    num,
                    Style::default().fg(theme.text_ghost).bg(bg),
                ));
                let text = truncate(&s.text, side);
                let pad = " ".repeat(side.saturating_sub(disp_width(&text)));
                let style = Style::default().fg(fg).bg(bg);
                // The part that actually differs, in the two-tone scheme every
                // diff a reader already knows uses: the line keeps its tint and
                // the span deepens it, with the text lifted rather than
                // inverted. Reversing it would turn the words most worth
                // reading into the darkest text on the row, and next to a
                // ground that is already coloured it reads as a selection.
                //
                // Only while the line is whole: a truncated line's byte offsets
                // no longer point at what they were measured against.
                match s.emph.filter(|(_, e)| *e <= text.len() && text == s.text) {
                    Some((a, b)) => {
                        let mark = Style::default()
                            .fg(mix(tint, Color::Rgb(255, 255, 255), DIFF_SPAN_LIFT))
                            .bg(mix(theme.surface, tint, DIFF_SPAN_TINT))
                            .bold();
                        spans.push(Span::styled(text[..a].to_string(), style));
                        spans.push(Span::styled(text[a..b].to_string(), mark));
                        spans.push(Span::styled(text[b..].to_string(), style));
                    }
                    None => spans.push(Span::styled(text, style)),
                }
                spans.push(Span::styled(pad, style));
                spans
            };
            let mut spans = cell(old.as_ref(), left, theme.failure);
            spans.push(Span::styled(
                DIFF_RULE,
                Style::default().fg(theme.border_dim),
            ));
            spans.extend(cell(new.as_ref(), right, theme.success));
            Line::from(spans)
        }
    }
}

/// One unified diff line as a drawn line — the narrow-terminal layout.
/// `wipe` and `icon` as in `diff_row_line`.
fn diff_unified_line(
    l: &DiffLine,
    emph: Option<ByteSpan>,
    width: usize,
    wipe: f64,
    icon: &str,
    theme: &Theme,
) -> Line<'static> {
    match l {
        DiffLine::File { path, add, del } => {
            file_banner(path, *add, *del, width, wipe, icon, theme)
        }
        DiffLine::Section(label) => Line::from(Span::styled(
            format!("── {label} "),
            Style::default().fg(theme.accent).bold(),
        )),
        DiffLine::Text(t) => {
            let style = diff_line_style(t, theme);
            // The line colour says added/removed; on a paired ±line the
            // reverse-video span says *where* — the same scheme as git's
            // own diff-highlight, so it needs no theme colour of its own.
            match emph {
                Some((s, e)) if e <= t.len() => Line::from(vec![
                    Span::styled(t[..s].to_string(), style),
                    Span::styled(t[s..e].to_string(), style.add_modifier(Modifier::REVERSED)),
                    Span::styled(t[e..].to_string(), style),
                ]),
                _ => Line::from(Span::styled(t.clone(), style)),
            }
        }
    }
}

/// The diff for one file from the working-tree view.
///
/// Side by side wherever the terminal can hold two columns: a diff is a
/// comparison, and reading one as a single column asks you to hold the old
/// line in your head while you scroll to the new one. The unified layout is
/// kept for terminals too narrow to split.
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
    // The names live on the bands the pane scrolls through — every file has
    // one now — so the title counts files rather than repeating the first
    // one two rows above itself. Until the diff arrives there are no bands
    // yet, and the title is the only thing that can say what is coming.
    let mut title = if dv.loading || dv.files.is_empty() {
        format!("{}  ", dv.file)
    } else if dv.files.len() == 1 {
        "1 file  ".to_string()
    } else {
        format!("{} files  ", dv.files.len())
    };
    if dv.loading {
        title.push_str("loading…");
    } else {
        title.push_str(&format!("+{add} −{del}"));
    }

    if dv.lines.is_empty() {
        let msg = if dv.loading {
            "reading diff…"
        } else {
            // Mode changes and pure renames are real status entries with no
            // textual diff at all — say so rather than showing a blank pane.
            "no textual changes (mode change, rename, or an empty file)"
        };
        dv.units.set(0);
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(theme.text_faint).italic()))
                .block(styled_block(&title, theme)),
            area,
        );
        return;
    }

    let inner_w = area.width.saturating_sub(2) as usize;
    let split = diff_columns(inner_w, &dv.rows);

    // What the scroll offset counts, and what "how much is off-screen" means:
    // rows when there are two columns, lines when there is one. Handed back to
    // the key handler so paging stops where the eye does.
    let total = match split {
        Some(_) => dv.rows.len(),
        None => dv.lines.len(),
    };
    dv.units.set(total);
    dv.side_by_side.set(split.is_some());

    // A jump waiting on the layout — opening on a file, `n`/`p` — resolves
    // here, where which unit the offset counts is finally known.
    if let Some(fi) = dv.pending_jump.take() {
        let starts = if split.is_some() { &dv.file_rows } else { &dv.file_lines };
        if let Some(&target) = starts.get(fi) {
            dv.scroll.set(target.min(dv.max_scroll(viewport as usize)));
        }
    }

    // Position, so a long diff says how much of it is off-screen.
    let scroll = dv.scroll.get().min(total.saturating_sub(1));
    if total > viewport as usize {
        let last = (scroll + viewport as usize).min(total);
        title.push_str(&format!("   [{}–{} of {}]", scroll + 1, last, total));
    }

    // Slicing to the viewport keeps the per-frame clone bounded rather than
    // copying a 10k-line diff on every redraw.
    // A band's entrance progress, looked up as the render passes over it —
    // which is also what marks it as seen for the first time.
    let wipe_for = |path: &str| {
        dv.files
            .iter()
            .position(|f| f == path)
            .map_or(1.0, |fi| band_wipe_at(dv, fi, state.tick_count))
    };
    let icon_for = |path: &str| if state.file_icons { lang_icon(path) } else { "" };
    let lines: Vec<Line> = match split {
        Some((gutter, left, right)) => dv.rows[scroll..]
            .iter()
            .take(viewport as usize)
            .map(|r| {
                let (wipe, icon) = match r {
                    DiffRow::File { path, .. } => (wipe_for(path), icon_for(path)),
                    _ => (1.0, ""),
                };
                diff_row_line(r, gutter, left, right, wipe, icon, theme)
            })
            .collect(),
        None => dv.lines[scroll..]
            .iter()
            .take(viewport as usize)
            .enumerate()
            .map(|(i, l)| {
                let (wipe, icon) = match l {
                    DiffLine::File { path, .. } => (wipe_for(path), icon_for(path)),
                    _ => (1.0, ""),
                };
                diff_unified_line(
                    l,
                    dv.emphasis.get(scroll + i).copied().flatten(),
                    inner_w,
                    wipe,
                    icon,
                    theme,
                )
            })
            .collect(),
    };

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
    register_table_hits(
        state,
        inner,
        2,
        1,
        state.workflows.len(),
        state.workflow_cursor,
        |r| Some(Hit::Workflow(r)),
    );
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
    // Runs rows are two lines tall (branch over commit message).
    register_table_hits(state, inner, 2, 2, state.runs.len(), state.run_cursor, |r| {
        Some(Hit::Run(r))
    });
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

    // The failure digest: the part of the log worth reading, above the step
    // list rather than behind it. Only for the run on screen, only while it is
    // red, and only when showing it still leaves the steps a useful share.
    let digest = state
        .failure_digest
        .as_ref()
        .filter(|d| d.run_id == detail.run.id && detail.run.status.is_failure());
    let digest_h = digest
        .map(|d| d.lines.len() as u16 + 2)
        .filter(|h| inner.height >= h + 8)
        .unwrap_or(0);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(digest_h),
            Constraint::Min(0),
        ])
        .split(inner);

    if let (Some(d), true) = (digest, digest_h > 0) {
        let what = d.step_name.as_deref().unwrap_or(d.job_name.as_str());
        let dblk = Block::default()
            .title(Span::styled(
                format!(" why it failed — {} ", truncate(what, 48)),
                Style::default().fg(theme.failure).bold(),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.failure_dim));
        let dinner = dblk.inner(inner_chunks[1]);
        f.render_widget(dblk, inner_chunks[1]);
        let lines: Vec<Line> = d
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let base = if d.error_rows.contains(&i) {
                    Style::default().fg(theme.failure).bold()
                } else {
                    Style::default().fg(theme.text_muted)
                };
                Line::from(ansi_line_to_spans(l, base))
            })
            .collect();
        f.render_widget(Paragraph::new(lines), dinner);
    }

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
    f.render_widget(table, inner_chunks[2]);
    // This table never scrolls (drawn stateless from row 0), so the click map
    // is pinned to the top with `selected_row = 0`.
    register_table_hits(state, inner_chunks[2], 0, 1, items.len(), 0, |r| {
        Some(Hit::DetailItem(r))
    });
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
    // The live tail earns a pane only while a job is actually producing one —
    // between runs, and once everything has settled, the steps get the room
    // back and the view is exactly what it was before the pane existed.
    let tail_job = state
        .run_detail
        .as_ref()
        .filter(|d| !d.run.status.is_terminal())
        .and_then(|d| d.jobs.iter().find(|j| j.status == Status::Running));
    let tail_h = if tail_job.is_some() && area.height >= 20 {
        (area.height / 3).clamp(5, 14)
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(0),
            Constraint::Length(tail_h),
        ])
        .split(area);

    // Drawn before the step lists, whose section returns early when a run has
    // no jobs yet — which is exactly when "waiting for the log" is the only
    // thing worth saying.
    if let Some(job) = tail_job
        && tail_h > 0
    {
        let ta = chunks[2];
        let viewport = ta.height.saturating_sub(2) as usize;
        let (title, lines): (String, Vec<Line>) = match &state.watch_tail {
            Some(t) if t.job_id == job.id && !t.lines.is_empty() => {
                // The tail follows the newest lines by definition; scrolling
                // back through history is what the Logs view is for.
                let shown: Vec<Line> = t
                    .lines
                    .iter()
                    .rev()
                    .take(viewport)
                    .rev()
                    .map(|l| {
                        Line::from(ansi_line_to_spans(
                            l,
                            Style::default().fg(theme.text),
                        ))
                    })
                    .collect();
                (
                    format!("Live log — {}  ({} lines)", t.job_name, t.lines.len()),
                    shown,
                )
            }
            // GitHub withholds a running job's log until it ends, so the
            // pane shows the thing that does move: the job's own steps.
            _ => {
                let steps = live_step_lines(job, viewport, state.tick_count, theme);
                let body = if steps.is_empty() {
                    vec![Line::from(Span::styled(
                        "waiting for the first step…",
                        Style::default().fg(theme.text_muted).italic(),
                    ))]
                } else {
                    steps
                };
                (format!("Live steps — {}", job.name), body)
            }
        };
        f.render_widget(
            Paragraph::new(lines).block(styled_block(&title, &state.theme)),
            ta,
        );
    }

    let summary_lines = if let Some(detail) = &state.run_detail {
        let secs = elapsed_seconds(&detail.run);
        let elapsed = format_elapsed(secs);
        let step = detail.current_step().unwrap_or("—");
        // What the recent runs say this workflow usually takes, and what that
        // makes of the clock: "usually 3:57 · ~1:16 left" turns a stopwatch
        // into an answer to the question actually being asked of it.
        let mut elapsed_spans = vec![
            Span::styled("Elapsed:", Style::default().fg(theme.primary).bold()),
            Span::raw(format!(" {}", elapsed)),
        ];
        if !detail.run.status.is_terminal()
            && let Some(t) = typical_run_secs(state.runs.iter(), &detail.run.display_title)
        {
            elapsed_spans.push(Span::styled(
                format!("  ·  usually {}", format_elapsed(t)),
                Style::default().fg(theme.text_muted),
            ));
            elapsed_spans.push(Span::styled(
                format!("  ·  {}", eta_text(t, secs)),
                Style::default().fg(theme.warning),
            ));
        }
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
            Line::from(elapsed_spans),
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

/// The typical duration of one workflow, learned from the finished successes
/// already on hand — the median, which one freak cache-miss build cannot
/// drag. `None` until there is at least one to learn from.
///
/// Runs, not the history file: every caller already holds a window of this
/// workflow's recent runs with both timestamps on them, which is fresher than
/// anything on disk and exists for remote-only repos too.
fn typical_run_secs<'a>(runs: impl Iterator<Item = &'a Run>, title: &str) -> Option<i64> {
    let mut durs: Vec<i64> = runs
        .filter(|r| r.display_title == title && r.status == Status::Success)
        .map(|r| (r.updated_at - r.created_at).num_seconds())
        .filter(|s| *s > 0)
        .collect();
    if durs.is_empty() {
        return None;
    }
    durs.sort_unstable();
    Some(durs[durs.len() / 2])
}

/// What to say about a running run's remaining time. Honest about being a
/// guess — past the typical it says so rather than counting negative time.
fn eta_text(typical: i64, elapsed: i64) -> String {
    let left = typical - elapsed;
    if left >= 5 {
        format!("~{} left", format_elapsed(left))
    } else {
        "running long".into()
    }
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
    fn the_help_card_plays_an_entrance_then_settles() {
        let mut st = noisy_log_state(false);
        st.show_help = true;
        st.help_opened_tick = Some(100);
        st.tick_count = 100;
        assert!(help_revealing(&st));
        assert_eq!(help_reveal_at(&st, 0), 0.0, "nothing has arrived yet");
        st.tick_count = 103;
        assert_eq!(help_reveal_at(&st, 0), 1.0, "the first row has landed");
        assert!(help_reveal_at(&st, 20) < 1.0, "later rows are still arriving");
        st.tick_count = 100 + HELP_REVEAL_HORIZON;
        assert!(!help_revealing(&st), "the entrance ends");
        // Without an opening tick (a redraw, a test), the card is simply
        // settled rather than frozen on its first frame.
        st.help_opened_tick = None;
        assert_eq!(help_reveal_at(&st, 30), 1.0);
    }

    fn diff_state(text: &str) -> AppState {
        let mut st = noisy_log_state(false);
        let mut dv = crate::app::state::GitDiffView::new("acme/api".into(), "src/api.rs".into());
        dv.set_sections(vec![crate::git::DiffSection { label: "unstaged", text: text.into() }]);
        st.git_diff = Some(dv);
        st.view = View::GitDiff;
        st
    }

    fn draw_diff(st: &AppState, w: u16, h: u16) -> String {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_git_diff(f, f.area(), st)).unwrap();
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

    /// The hunk used by the diff tests: one line replaced, one added, context
    /// either side.
    const HUNK: &str = "@@ -12,7 +12,8 @@ fn main() {\n     let cfg = Config::load()?;\n-    let x = 1;\n+    let x = 2;\n+    log::info!(\"done\");\n     Ok(())\n";

    #[test]
    fn a_diff_reads_across_rather_than_down() {
        let st = diff_state(HUNK);
        let out = draw_diff(&st, 100, 12);
        // Without the pane's own left border, so the first `│` left in the row
        // is the rule between the two columns.
        let row = out
            .lines()
            .find(|l| l.contains("let x = 1;"))
            .unwrap_or_else(|| panic!("no row for the old line:\n{out}"))
            .trim_start_matches('│');
        // The whole point: the line that was and the line that is are the same
        // row, so the comparison is a glance sideways and not a memory test.
        assert!(row.contains("let x = 2;"), "got {row:?}");
        // Each side numbered in its own file: line 13 became line 13.
        assert!(row.starts_with("13 "), "no old line number: {row:?}");
        assert!(row.contains("│ 13 "), "no new line number: {row:?}");

        // A line with no counterpart leaves the other side empty rather than
        // shifting everything below it out of step.
        let added = out
            .lines()
            .find(|l| l.contains("log::info!"))
            .unwrap()
            .trim_start_matches('│');
        let (before, _) = added.split_once('│').unwrap();
        assert_eq!(before.trim(), "", "the gap opposite an addition was filled");

        // Context is carried by both sides, so the eye never loses the file.
        assert!(out.lines().any(|l| l.matches("Ok(())").count() == 2), "{out}");
    }

    #[test]
    fn the_word_that_changed_is_still_marked_inside_the_pair() {
        // Its own hunk: emphasis is only claimed for runs that pair up one for
        // one, and the shared one adds a line as well as changing one.
        let st = diff_state("@@ -13 +13 @@\n-    let x = 1;\n+    let x = 2;\n");
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        term.draw(|f| render_git_diff(f, f.area(), &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let cells = || (0..buf.area.height).flat_map(|y| (0..buf.area.width).map(move |x| (x, y)));
        // The row holding the pair — the title and the hunk header are bold in
        // their own right, and neither is what is being asked about here.
        let row = (0..buf.area.height)
            .find(|&y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("let x = 1;")
            })
            .unwrap();

        // The only difference between the two lines is the digit, so exactly
        // the digits carry the mark — one on each side of the rule.
        let marked: Vec<(u16, u16)> = (0..buf.area.width)
            .map(|x| (x, row))
            .filter(|&p| buf[p].modifier.contains(Modifier::BOLD))
            .collect();
        let text: String = marked.iter().map(|&p| buf[p].symbol().to_string()).collect();
        assert_eq!(text, "12", "got {text:?}");

        // Two tones of one hue, not an inversion: the span's ground is deeper
        // than the line's, and its text is lifted rather than swapped with the
        // ground — reversing it would make the words most worth reading the
        // darkest on the row.
        let (mx, my) = marked[0];
        let row_bg = buf[(mx - 1, my)].bg;
        assert_ne!(buf[(mx, my)].bg, row_bg, "the span sits on the line's own ground");
        assert_ne!(buf[(mx, my)].fg, buf[(mx - 1, my)].fg, "the span's text was not lifted");
        assert!(
            !cells().any(|p| buf[p].modifier.contains(Modifier::REVERSED)),
            "reverse video is for the unified fallback, which has no tint to deepen"
        );
    }

    #[test]
    fn a_terminal_too_narrow_to_split_gets_the_unified_diff() {
        let st = diff_state(HUNK);
        let out = draw_diff(&st, 44, 12);
        // No rule down the middle, and the markers that carried the meaning in
        // one column are back.
        assert!(!out.contains(" │ "), "still split at 44 columns:\n{out}");
        assert!(out.contains("-    let x = 1;"), "{out}");
        assert!(out.contains("+    let x = 2;"), "{out}");
    }

    #[test]
    fn a_lone_file_gets_the_same_band_as_any_other() {
        let st = diff_state(HUNK);
        let out = draw_diff(&st, 100, 12);
        // The band names the file inside the pane…
        let band = out.lines().nth(1).unwrap();
        assert!(band.contains("src/api.rs  +2 −1"), "got {band:?}");
        // …so the title counts files instead of repeating the name above it.
        assert!(out.starts_with("╭─┤ 1 file  +2 −1 ├"), "got:\n{out}");
        assert!(!out.lines().next().unwrap().contains("api.rs"), "got:\n{out}");
    }

    #[test]
    fn the_scroll_counts_what_the_layout_actually_drew() {
        let st = diff_state(HUNK);
        let dv = st.git_diff.as_ref().unwrap();
        // The file's band, the blank under it, and six unified lines — of
        // which the ± pair folds into one row.
        assert_eq!(dv.lines.len(), 8);
        assert_eq!(dv.rows.len(), 7);

        draw_diff(&st, 100, 12);
        assert_eq!(dv.units.get(), 7, "split: the offset counts rows");
        assert_eq!(dv.max_scroll(3), 4);

        draw_diff(&st, 44, 12);
        assert_eq!(dv.units.get(), 8, "unified: it counts lines again");
        assert_eq!(dv.max_scroll(3), 5);
    }

    #[test]
    fn the_combined_diff_opens_at_the_chosen_file_under_its_own_banner() {
        let sec = |text: &str| crate::git::DiffSection { label: "unstaged", text: text.into() };
        let long: String = (0..40).map(|i| format!("+l{i}\n")).collect();
        let mut st = noisy_log_state(false);
        let mut dv =
            crate::app::state::GitDiffView::new("acme/api".into(), "src/b.rs".into());
        dv.set_files(vec![
            ("src/a.rs".into(), vec![sec(&format!("@@ -1 +1,40 @@\n{long}"))]),
            ("src/b.rs".into(), vec![sec(&format!("@@ -1 +1,40 @@\n{long}\n-x\n"))]),
        ]);
        st.git_diff = Some(dv);
        st.view = View::GitDiff;
        let out = draw_diff(&st, 100, 20);

        // The first frame resolves the jump: `d` was pressed on `src/b.rs`,
        // so the view opens scrolled to it rather than at the top of `a`.
        let dv = st.git_diff.as_ref().unwrap();
        assert_eq!(dv.pending_jump.get(), None, "the jump was consumed");
        assert!(dv.scroll.get() > 0, "still at the top of the first file");
        assert_eq!(dv.current_file().as_deref(), Some("src/b.rs"));

        // Each file opens under a full-width band naming it with its own +/−
        // counts.
        let band = out
            .lines()
            .find(|l| l.contains("src/b.rs"))
            .unwrap_or_else(|| panic!("no band for src/b.rs:\n{out}"));
        assert!(band.contains("src/b.rs  +40 −1"), "got {band:?}");
    }

    #[test]
    fn a_file_band_sweeps_in_when_first_seen_then_settles() {
        let sec = |text: &str| crate::git::DiffSection { label: "unstaged", text: text.into() };
        let mut st = noisy_log_state(false);
        let mut dv =
            crate::app::state::GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_files(vec![
            ("src/a.rs".into(), vec![sec("@@ -1 +1 @@\n+one\n")]),
            ("src/b.rs".into(), vec![sec("@@ -1 +1 @@\n+two\n")]),
        ]);
        // The clock only runs inside the app's event loop; without it (every
        // other test here) the bands draw settled.
        dv.opened_tick = Some(0);
        st.git_diff = Some(dv);
        st.view = View::GitDiff;

        // Widest run of the accent ground on any row — how far the sweep got.
        let band_cols = |st: &AppState| -> usize {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
            term.draw(|f| render_git_diff(f, f.area(), st)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).filter(|&x| buf[(x, y)].bg == st.theme.accent).count())
                .max()
                .unwrap_or(0)
        };

        st.tick_count = 0;
        let partial = band_cols(&st);
        assert!(partial > 0, "the sweep starts with something on screen");
        assert!(partial < 90, "the first frame already spans the pane: {partial}");
        assert!(diff_bands_revealing(&st), "mid-sweep, the loop owes frames");

        st.tick_count = 30;
        let full = band_cols(&st);
        assert_eq!(full, 98, "settled, the band runs edge to edge");
        assert!(!diff_bands_revealing(&st), "settled, it stops asking for them");
    }

    #[test]
    fn the_band_wears_its_languages_mark() {
        assert_eq!(lang_icon("src/app/state.rs"), "\u{e7a8}");
        assert_eq!(lang_icon("scripts/run.py"), "\u{e73c}");
        assert_eq!(lang_icon("web/App.tsx"), "\u{e628}");
        assert_eq!(lang_icon("Dockerfile"), "\u{f308}");
        assert_eq!(lang_icon("Cargo.lock"), "\u{f023}");
        // Unrecognised gets the plain file — the honest mark for it.
        assert_eq!(lang_icon("LICENSE"), "\u{f016}");

        let sec = |text: &str| crate::git::DiffSection { label: "unstaged", text: text.into() };
        let mut st = noisy_log_state(false);
        let mut dv =
            crate::app::state::GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_files(vec![
            ("src/a.rs".into(), vec![sec("@@ -1 +1 @@\n+one\n")]),
            ("src/b.py".into(), vec![sec("@@ -1 +1 @@\n+two\n")]),
        ]);
        st.git_diff = Some(dv);
        st.view = View::GitDiff;
        let out = draw_diff(&st, 100, 20);
        assert!(out.contains("\u{e7a8} src/a.rs"), "no rust mark:\n{out}");
        assert!(out.contains("\u{e73c} src/b.py"), "no python mark:\n{out}");

        // A terminal without the font can turn the marks off, same as the
        // forge icon.
        st.file_icons = false;
        let out = draw_diff(&st, 100, 20);
        assert!(!out.contains('\u{e7a8}'), "mark still drawn when off:\n{out}");
        assert!(out.contains(" src/a.rs"), "the name must survive the mark:\n{out}");
    }

    #[test]
    fn the_dashboard_arrives_from_the_top_and_then_settles() {
        let mut st = noisy_log_state(false);
        st.view = View::Repos;
        st.tick_count = 100;
        st.dash_opened_tick = Some(100);
        assert!(dash_revealing(&st));
        assert_eq!(dash_reveal_at(&st, 0), 0.0, "nothing has arrived yet");
        st.tick_count = 103;
        assert_eq!(dash_reveal_at(&st, 0), 1.0, "the first line has landed");
        assert!(dash_reveal_at(&st, 20) < 1.0, "later lines are still arriving");
        st.tick_count = 200;
        assert!(!dash_revealing(&st), "the entrance ends");
        // A dashboard on screen without the clock having been started (a test,
        // a direct `view = Repos`) is settled rather than frozen dark.
        st.dash_opened_tick = None;
        assert_eq!(dash_reveal_at(&st, 30), 1.0);
    }

    #[test]
    fn the_dashboards_first_frame_is_ground_and_its_empty_space_is_left_alone() {
        let mut st = noisy_log_state(false);
        for spec in ["acme/api", "acme/web"] {
            let mut card = crate::app::state::RepoCard::new(spec.into());
            card.path = Some(std::path::PathBuf::from("/tmp").join(spec));
            card.git = Some(crate::git::parse_status("## main\0"));
            card.loaded = true;
            st.repos.push(card);
        }
        st.view = View::Repos;
        st.tick_count = 100;
        st.dash_opened_tick = Some(100);

        let draw = |st: &AppState| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
            term.draw(|f| render_repos(f, f.area(), st)).unwrap();
            term.backend().buffer().clone()
        };
        let row_of = |buf: &ratatui::buffer::Buffer, needle: &str| -> u16 {
            (0..buf.area.height)
                .find(|&y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("no row for {needle}"))
        };

        // Frame one: the rows are on screen but painted in the dashboard's own
        // ground, so the table reads as empty and then fills in.
        let buf = draw(&st);
        let y = row_of(&buf, "acme/api");
        let name_x = (0..buf.area.width)
            .find(|&x| buf[(x, y)].symbol() == "a")
            .unwrap();
        assert_eq!(buf[(name_x, y)].fg, st.theme.row_idle, "the name has not arrived");
        // The space under the last row belongs to the terminal, not to the
        // sweep: blending it would flash a slab of ground and take it away.
        let below = buf.area.height - 1;
        assert_eq!(buf[(2, below)].bg, Color::Reset, "empty space was painted");

        // And once the entrance is over the row is its own colour again.
        st.tick_count = 200;
        let buf = draw(&st);
        let y = row_of(&buf, "acme/api");
        assert_ne!(buf[(name_x, y)].fg, st.theme.row_idle, "the name never landed");
    }

    #[test]
    fn the_help_card_stands_on_its_own_ground_and_admits_to_scrolling() {
        let mut st = noisy_log_state(false);
        st.show_help = true; // no opening tick: drawn settled
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 24)).unwrap();
        term.draw(|f| render_help_overlay(f, f.area(), &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let out: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("? Help — jog v"), "chip title: {out}");
        // Short terminal + long reference: the card must say how to move.
        assert!(out.contains("wheel scroll"), "{out}");
        // And the render told the key handler where the bottom is.
        assert!(st.last_help_max_scroll.get() > 0);
    }

    #[test]
    fn click_targets_match_the_rows_the_frame_would_draw() {
        let st = noisy_log_state(false);
        let inner = Rect { x: 2, y: 3, width: 60, height: 12 };
        // 30 rows, 10 visible under a 2-row header, cursor at 20: the window
        // must scroll exactly far enough for row 20 to be the bottom row.
        register_table_hits(&st, inner, 2, 1, 30, 20, |r| Some(Hit::Workflow(r)));
        let hits = st.hits.borrow();
        assert_eq!(hits.len(), 10);
        assert_eq!(hits.first().unwrap().1, Hit::Workflow(11));
        assert_eq!(hits.first().unwrap().0.y, inner.y + 2);
        assert_eq!(hits.last().unwrap().1, Hit::Workflow(20));
        assert_eq!(hits.last().unwrap().0.y, inner.y + 11);
        drop(hits);

        // A short list under a tall viewport never scrolls, whatever is
        // selected — and two-row rows step their rects by two.
        st.hits.borrow_mut().clear();
        register_table_hits(&st, inner, 2, 2, 3, 2, |r| Some(Hit::Run(r)));
        let hits = st.hits.borrow();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].1, Hit::Run(0));
        assert_eq!(hits[1].0.y, inner.y + 4);
        assert_eq!(hits[1].0.height, 2);
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
    fn a_failed_hook_writes_the_way_out_on_the_pane_itself() {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        op.push_line("pytest...Failed".into(), false);
        op.finished = true;
        op.failed = true;
        let out = draw_changes(&state_with_op(op), 100, 14);
        // Reading it, copying it, and putting it away — plus the two keys that
        // get the commit made once the hook's complaint has been dealt with.
        for want in ["j/k scroll", "e/E error", "y yank", "Esc dismiss", "a stage all", "c commit again"] {
            assert!(out.contains(want), "no `{want}` on the pane:\n{out}");
        }
    }

    #[test]
    fn a_running_hook_offers_only_what_it_can_actually_do() {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        op.push_line("ruff.....".into(), false);
        let out = draw_changes(&state_with_op(op), 100, 14);
        // `back` leaves the command running rather than killing it, and there
        // is nothing to fix or re-commit until it has had its say.
        assert!(out.contains("Esc leave, keeps running"), "got:\n{out}");
        assert!(!out.contains("commit again"), "got:\n{out}");
        // No errors yet, so no error walk offered.
        assert!(!out.contains("e/E error"), "got:\n{out}");
    }

    #[test]
    fn the_batch_pane_leaves_retry_and_skip_to_the_queue_above_it() {
        let out = draw_batch(&batch_state(BatchPhase::Paused), 110, 18);
        // Said once, on the queue that owns those keys.
        assert_eq!(out.matches("retry").count(), 1, "got:\n{out}");
        assert_eq!(out.matches("skip").count(), 1, "got:\n{out}");
        // The pane still says what the pane can do.
        assert!(out.contains("y yank"), "got:\n{out}");
    }

    #[test]
    fn a_narrow_pane_drops_whole_keys_rather_than_half_of_one() {
        let theme = Theme::midnight();
        let rail = vec![
            ("j/k".to_string(), "scroll"),
            ("y".to_string(), "yank"),
            ("Esc".to_string(), "dismiss"),
        ];
        let text = |w| -> String {
            rail_line(&rail, w, &theme)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(text(60).trim(), "j/k scroll · y yank · Esc dismiss");
        // Room for two: the third goes whole, taking its separator with it.
        assert_eq!(text(24).trim(), "j/k scroll · y yank");
        // Room for none: nothing at all, rather than a stray fragment.
        assert_eq!(text(8).trim(), "");
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

    #[test]
    fn the_services_card_names_every_monitor_mapped_or_not() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.show_services = true;
        st.services = vec![
            crate::kuma::Service {
                name: "API".into(),
                state: crate::kuma::ServiceState::Up,
                ping_ms: Some(44),
                uptime24: Some(1.0),
                group: "production".into(),
                tags: vec!["critical".into()],
            },
            crate::kuma::Service {
                name: "Logs UI".into(),
                state: crate::kuma::ServiceState::Down,
                ping_ms: None,
                uptime24: Some(0.97),
                group: "stage".into(),
                tags: Vec::new(),
            },
        ];
        st.service_repos.insert("API".into(), "acme/backend".into());

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        term.draw(|f| render_services_overlay(f, f.area(), &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let out = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("API") && out.contains("44ms"), "{out}");
        // The groups are the production/stage answer; tags are the other one.
        assert!(out.contains("production") && out.contains("stage"), "{out}");
        assert!(out.contains("#critical"), "{out}");
        assert!(out.contains("→ acme/backend"), "mapped monitors say whose row they sit on\n{out}");
        // The unmapped monitor is exactly the one only this card can show.
        assert!(out.contains("Logs UI") && out.contains("down"), "{out}");
        assert!(out.contains("97.0% today"), "{out}");
    }

    #[test]
    fn one_tag_worn_by_every_row_becomes_a_heading_instead() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.show_services = true;
        let svc = |name: &str, tags: Vec<String>| crate::kuma::Service {
            name: name.into(),
            state: crate::kuma::ServiceState::Up,
            ping_ms: Some(40),
            uptime24: Some(1.0),
            // One status-page group for everyone: the case where the tags are
            // the only structure there is.
            group: "Services".into(),
            tags,
        };
        st.services = vec![
            svc("API", vec!["Prod".into()]),
            svc("web", vec!["Prod".into(), "critical".into()]),
            svc("Stage", Vec::new()),
        ];
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 14)).unwrap();
        term.draw(|f| render_services_overlay(f, f.area(), &st)).unwrap();
        let buf = term.backend().buffer().clone();
        let out = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            out.contains("▐ Prod ▌"),
            "the shared tag heads its section, as a chip\n{out}"
        );
        assert!(!out.contains("#Prod"), "…and is not repeated on every row\n{out}");
        assert!(out.contains("#critical"), "other tags stay as chips\n{out}");
        assert!(out.contains("▐ untagged ▌"), "the tagless get a section, at the end\n{out}");
    }

    #[test]
    fn the_verdicts_arrive_one_row_at_a_time_and_then_stand_still() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.show_services = true;
        let svc = |name: &str, state| crate::kuma::Service {
            name: name.into(),
            state,
            ping_ms: Some(40),
            uptime24: Some(1.0),
            group: "Services".into(),
            tags: Vec::new(),
        };
        st.services = vec![
            svc("API", crate::kuma::ServiceState::Up),
            svc("site", crate::kuma::ServiceState::Down),
        ];
        let card = |st: &AppState| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 10)).unwrap();
            term.draw(|f| render_services_overlay(f, f.area(), st)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // A card put up without the clock is a settled card, not a first frame:
        // every other test here draws one, and so does a redraw after a resize.
        assert!(card(&st).contains("up"), "no opening tick, nothing to play");

        // The frame the key was pressed on: names are already in place, so the
        // card does not resize under the reveal, but no verdict has landed.
        st.tick_count = 100;
        st.services_opened_tick = Some(100);
        let opening = card(&st);
        assert!(opening.contains("API") && opening.contains("site"), "{opening}");
        assert!(!opening.contains("down"), "the verdict is not there yet\n{opening}");

        // Mid-entrance the top row has said its piece and the one under it has
        // not — that stagger *is* the animation.
        st.tick_count = 102;
        let mid = card(&st);
        assert!(mid.contains(" up "), "the first row is in\n{mid}");
        assert!(!mid.contains("down"), "the second is still coming\n{mid}");
        assert!(services_revealing(&st), "and the loop keeps redrawing for it");

        // Then it stops: a card that kept animating would pull the eye back to
        // it for as long as it was open.
        st.tick_count = 120;
        assert!(card(&st).contains("down"), "everyone has arrived");
        assert!(!services_revealing(&st), "so the redraws stop");
        st.show_services = false;
        assert!(!services_revealing(&st), "a closed card animates nothing");
    }

    #[test]
    fn an_environment_wears_its_own_colour() {
        let theme = Theme::midnight();
        // Green is the one you don't touch, amber the one you do — read at a
        // glance or not at all.
        assert_eq!(env_color("production", &theme), theme.success);
        assert_eq!(env_color("Prod", &theme), theme.success);
        assert_eq!(env_color("staging", &theme), theme.warning);
        assert_eq!(env_color("Stage", &theme), theme.warning);
        assert_eq!(env_color("dev", &theme), theme.info);
        // A name with no convention behind it still gets a chip: an unlabelled
        // section reads as a bug, not as "no environment".
        assert_eq!(env_color("customers", &theme), theme.accent);
        assert_eq!(env_color("untagged", &theme), theme.unknown);

        // And the fill is deepened until white text can sit on it: the palette's
        // own success is a pastel meant for glyphs, not for backgrounds.
        let (fill, ink) = chip_colors(theme.success);
        assert_eq!(ink, Color::White);
        assert!(
            matches!((fill, theme.success), (Color::Rgb(a, ..), Color::Rgb(b, ..)) if a < b),
            "the chip is darker than the glyph colour it came from"
        );
    }

    #[test]
    fn the_heart_only_raises_its_voice_when_something_is_down() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let svc = |name: &str, state| crate::kuma::Service {
            name: name.into(),
            state,
            ping_ms: Some(40),
            uptime24: Some(1.0),
            group: "Services".into(),
            tags: Vec::new(),
        };
        assert!(
            workspace_tallies(&st).is_empty(),
            "no Kuma configured, no heart at all"
        );
        st.services = vec![
            svc("API", crate::kuma::ServiceState::Up),
            svc("site", crate::kuma::ServiceState::Up),
        ];
        let text = |spans: Vec<Span>| spans.iter().map(|s| s.content.clone()).collect::<String>();
        assert!(text(workspace_tallies(&st)).contains("♥2"), "quietly all up");
        st.services[0].state = crate::kuma::ServiceState::Down;
        assert!(
            text(workspace_tallies(&st)).contains("♥1/2"),
            "one down is a fraction, not a calm total"
        );

        // And the banner's heart beats: filled on the beat, hollow between.
        st.tick_count = 0;
        assert!(text(now_playing(&st)).starts_with("♥"), "tick 0 is a beat");
        st.tick_count = 8;
        assert!(text(now_playing(&st)).starts_with("♡"), "tick 8 is the rest");
    }

    #[test]
    fn the_eta_is_the_median_of_this_workflows_successes() {
        let done = |mins: i64, status| {
            let mut r = a_run(0, "Deploy", status, 0);
            r.created_at = Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, 0).unwrap();
            r.updated_at = r.created_at + chrono::Duration::minutes(mins);
            r
        };
        let runs = vec![
            done(4, Status::Success),
            done(3, Status::Success),
            // Failures say nothing about how long a healthy run takes.
            done(60, Status::Failure),
            // The freak build is exactly what the median exists to shrug off.
            done(30, Status::Success),
        ];
        assert_eq!(typical_run_secs(runs.iter(), "Deploy"), Some(240));
        assert_eq!(
            typical_run_secs(runs.iter(), "Other"),
            None,
            "someone else's history predicts nothing"
        );
    }

    #[test]
    fn the_eta_stops_promising_when_the_run_outlives_its_history() {
        assert_eq!(eta_text(240, 100), "~2:20 left");
        // Past the typical, an honest guess is that there is no guess — never
        // a negative countdown.
        assert_eq!(eta_text(240, 238), "running long");
        assert_eq!(eta_text(240, 500), "running long");
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
        // `r` is the global refresh now; retry took the next free letter.
        assert!(out.contains("t retry"), "got:\n{out}");
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
    fn the_header_bell_tells_live_from_muted_from_configured_off() {
        use crate::app::state::{DEFAULT_BELL_ICON, DEFAULT_BELL_OFF_ICON};
        let mut st = dashboard_with_live_ci();
        let live = draw_header(&st, 120);
        assert!(live.contains(DEFAULT_BELL_ICON), "{live}");
        assert!(!live.contains(DEFAULT_BELL_OFF_ICON), "{live}");
        st.snooze_until = Some(Utc::now() + chrono::Duration::minutes(25));
        let muted = draw_header(&st, 120);
        assert!(muted.contains(DEFAULT_BELL_OFF_ICON), "{muted}");
        assert!(muted.contains("25m"), "{muted}");
        // Config that never announces wears no bell at all — not even a
        // slashed one, which would nag about a choice already made.
        st.snooze_until = None;
        st.notify_enabled = false;
        let off = draw_header(&st, 120);
        assert!(!off.contains(DEFAULT_BELL_ICON), "{off}");
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
    fn a_wide_dashboard_gets_a_live_pane_and_a_narrow_one_does_not() {
        let mut st = dashboard_with_live_ci();
        st.watch_tail = Some(crate::app::state::WatchTail {
            job_id: st.dash_tail_target().map(|(_, _, j)| j.id).unwrap(),
            job_name: "build".into(),
            lines: vec!["cargo build --release".into()],
        });
        let wide = draw_repos(&st, 170, 20);
        assert!(wide.contains("Live —"), "got:\n{wide}");
        assert!(wide.contains("cargo build --release"), "got:\n{wide}");
        // The table is still the view: its rows survive the split.
        assert!(wide.contains("muufree/website"), "got:\n{wide}");
        let narrow = draw_repos(&st, 150, 20);
        assert!(!narrow.contains("Live —"), "got:\n{narrow}");
        // With nothing in flight there is nothing to tail, however wide.
        st.run_progress.clear();
        let idle = draw_repos(&st, 170, 20);
        assert!(!idle.contains("Live —"), "got:\n{idle}");
    }

    #[test]
    fn with_no_log_body_the_live_pane_shows_the_steps_instead() {
        // GitHub does not serve a job's log until the job ends, so on a run in
        // flight the pane has no tail to show. It shows the steps, which do
        // move — the alternative was a "waiting…" line that never resolved.
        let st = dashboard_with_live_ci();
        assert!(st.watch_tail.is_none());
        let out = draw_repos(&st, 170, 20);
        assert!(out.contains("Live —"), "got:\n{out}");
        assert!(out.contains("Run migrations"), "got:\n{out}");
        assert!(!out.contains("waiting for GitHub"), "got:\n{out}");
        // Steps past the one in flight are not news yet.
        assert!(!out.contains("Push image"), "got:\n{out}");
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
        assert!(!text.contains("wheel scroll"), "nothing to scroll:\n{text}");
        assert!(text.contains("/ search"), "the filter is offered:\n{text}");
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
    fn typing_in_the_help_card_narrows_it_to_the_matching_bindings() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.view = View::GitStatus;
        st.show_help = true;
        st.help_search = "stage".into();
        let text = draw_help(&st, 120, 54);
        assert!(text.contains("stage all"), "the matching rows survive:\n{text}");
        // Rows that say nothing about staging are gone, and so are the
        // sections left with nothing in them.
        assert!(!text.contains("quit"), "unmatched rows are filtered out:\n{text}");
        assert!(!text.contains("Trigger prompt"), "empty sections drop out:\n{text}");
        assert!(text.contains("Esc clears"), "the filter says how to drop it:\n{text}");

        // With Global filtered away there is nothing to float under, so the
        // open view's section takes the top-left corner itself.
        st.help_search = "push".into();
        let text = draw_help(&st, 120, 54);
        let first = text
            .lines()
            .find(|l| !l.contains("─┤") && !l.trim_matches(['│', ' ']).is_empty())
            .unwrap();
        assert!(
            first.contains("Changes — working tree") && first.contains("you are here"),
            "the open view leads:\n{text}"
        );
    }

    #[test]
    fn every_term_of_a_help_search_has_to_land_somewhere_on_the_row() {
        assert!(help_row_matches("p", "push — sets upstream on first push", "push"));
        // Both words, in either order, anywhere in key or description.
        assert!(help_row_matches("p", "push — sets upstream on first push", "push upstream"));
        assert!(help_row_matches("p", "push — sets upstream on first push", "upstream push"));
        assert!(!help_row_matches("p", "push — sets upstream on first push", "push tag"));
        // The binding itself is searchable, and case never matters.
        assert!(help_row_matches("ctrl+p", "fuzzy find in the current list", "CTRL"));
    }

    #[test]
    fn a_help_search_that_matches_nothing_says_so() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.show_help = true;
        st.help_search = "xyzzy".into();
        let text = draw_help(&st, 120, 54);
        assert!(text.contains("no binding matches"), "an empty result explains itself:\n{text}");
    }

    #[test]
    fn the_search_terms_are_marked_in_the_rows_that_kept_them() {
        let base = Style::default();
        let hit = Style::default().bold();
        let spans = highlight_terms("stage / unstage the selected file", "stage", base, hit);
        let marked: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == hit)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, vec!["stage", "stage"], "both occurrences light up");
        // Nothing is lost or duplicated in the cutting.
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, "stage / unstage the selected file");
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
