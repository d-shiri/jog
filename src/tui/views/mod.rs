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
use crate::app::state::{AppState, DetailItem, GitOp, Theme, View, build_detail_items};
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
    }
    render_footer(f, chunks[2], state);
    if state.view == View::Logs {
        render_search_overlay(f, area, state);
    }
    if state.view == View::GitStatus {
        render_commit_overlay(f, area, state);
    }
    render_finder_overlay(f, area, state);
    // Drawn last so it sits above every other overlay.
    render_help_overlay(f, area, state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let sep = || Span::styled("  ›  ", Style::default().fg(Color::Rgb(55, 55, 80)));
    let crumb: Vec<Span> = match state.view {
        View::Repos => vec![
            Span::styled("Repos", Style::default().fg(theme.primary).bold()),
        ],
        View::GitStatus => vec![
            Span::styled("Repos", Style::default().fg(theme.secondary)),
            sep(),
            Span::styled(
                state
                    .git_view
                    .as_ref()
                    .map(|g| g.spec.clone())
                    .unwrap_or_else(|| "?".into()),
                Style::default().fg(theme.secondary),
            ),
            sep(),
            Span::styled("Changes", Style::default().fg(theme.primary).bold()),
        ],
        View::GitDiff => vec![
            Span::styled("Repos", Style::default().fg(theme.secondary)),
            sep(),
            Span::styled(
                state
                    .git_diff
                    .as_ref()
                    .map(|d| d.spec.clone())
                    .unwrap_or_else(|| "?".into()),
                Style::default().fg(theme.secondary),
            ),
            sep(),
            Span::styled("Changes", Style::default().fg(theme.secondary)),
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
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::Rgb(95, 95, 120)),
        ),
        Span::styled("  ·  ", dot),
    ];
    // On the workspace dashboard there is no single active repo yet — naming one
    // (and its branch) would be arbitrary. Show where we're scanning instead.
    match (&state.workspace_root, state.view) {
        (Some(root), View::Repos | View::GitStatus | View::GitDiff) => spans.push(Span::styled(
            format!("{}", root.display()),
            Style::default().fg(Color::White),
        )),
        _ => {
            spans.push(Span::styled(
                state.repo_label.as_str(),
                Style::default().fg(Color::White),
            ));
            spans.push(Span::styled("  ⎇ ", Style::default().fg(Color::Rgb(90, 110, 150))));
            spans.push(Span::styled(
                state.current_branch.as_str(),
                Style::default().fg(theme.accent),
            ));
        }
    }
    spans.push(Span::styled("  ·  ", dot));
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

    // The finder overlay owns the keyboard while it's up, so advertise its keys
    // instead of the view's.
    if state.finder.is_some() {
        let spans = vec![
            Span::raw(" "),
            Span::styled("type", Style::default().fg(Color::White).bold()),
            Span::raw(" "),
            Span::styled("filter", Style::default().fg(theme.secondary)),
            Span::raw("  "),
            Span::styled("↑/↓", Style::default().fg(Color::White).bold()),
            Span::raw(" "),
            Span::styled("move", Style::default().fg(theme.secondary)),
            Span::raw("  "),
            Span::styled("↵", Style::default().fg(Color::White).bold()),
            Span::raw(" "),
            Span::styled("select", Style::default().fg(theme.secondary)),
            Span::raw("  "),
            Span::styled("Esc", Style::default().fg(Color::White).bold()),
            Span::raw(" "),
            Span::styled("cancel", Style::default().fg(theme.secondary)),
        ];
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.footer_bg)),
            area,
        );
        return;
    }

    let hints: Vec<(String, &str)> = match state.view {
        View::Repos => vec![
            ("↵".into(), "open repo"),
            (display_key(&km.git_view).into(), "changes"),
            (display_key(&km.finder).into(), "find"),
            (display_key(&km.open_browser).into(), "open"),
            (display_key(&km.quit).into(), "quit"),
        ],
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

fn render_repos(f: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let (ok, fail, busy, broken) =
        state.repos.iter().fold((0u32, 0u32, 0u32, 0u32), |(o, f, r, b), c| {
            if c.error.is_some() {
                return (o, f, r, b + 1);
            }
            match c.latest_status() {
                Some(Status::Success) => (o + 1, f, r, b),
                Some(Status::Failure) => (o, f + 1, r, b),
                Some(Status::Running) | Some(Status::Queued) => (o, f, r + 1, b),
                _ => (o, f, r, b),
            }
        });
    let title = Line::from(vec![
        Span::styled(
            format!(" Repos ({}", state.repos.len()),
            Style::default().fg(theme.primary).bold(),
        ),
        Span::styled(format!("  ✓{ok}"), Style::default().fg(theme.success)),
        Span::styled(format!("  ✗{fail}"), Style::default().fg(theme.failure)),
        if busy > 0 {
            Span::styled(format!("  ⏵{busy}"), Style::default().fg(theme.warning).bold())
        } else {
            Span::raw("")
        },
        if broken > 0 {
            Span::styled(format!("  !{broken}"), Style::default().fg(theme.failure).bold())
        } else {
            Span::raw("")
        },
        Span::styled(" ) ", Style::default().fg(theme.primary).bold()),
    ]);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(55, 55, 80)))
        .title(title);
    let inner = blk.inner(area);
    f.render_widget(blk, area);

    if inner.height < 2 {
        return;
    }

    if state.repos.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no repos configured — add `repos = [\"owner/name\", …]` under [provider] in config.toml",
                Style::default().fg(Color::DarkGray).italic(),
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let hdr = Style::default().fg(theme.secondary);
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled("Repo", hdr)),
        Cell::from(Span::styled("Local branch", hdr)),
        Cell::from(Span::styled("Changes", hdr)),
        Cell::from(Span::styled("Latest run", hdr)),
        Cell::from(Span::styled("Ran on", hdr)),
        Cell::from(Span::styled("Updated", hdr)),
        Cell::from(Span::styled("Recent", hdr)),
    ])
    .height(1)
    .bottom_margin(1);

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
            Style::default().fg(Color::Yellow),
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
                Style::default().fg(Color::Rgb(120, 120, 145)),
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
                Span::styled("clean", Style::default().fg(Color::Rgb(90, 110, 95))),
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
            Style::default().fg(Color::Rgb(150, 130, 90))
        } else {
            Style::default().fg(Color::Yellow)
        };
        // The marker leads rather than trails: a long branch name would clip a
        // trailing one off the end of the column, and leading keeps the markers
        // aligned so divergent rows are scannable down the list.
        let marker = if diverged {
            Span::styled("≠ ", Style::default().fg(Color::Rgb(190, 160, 90)).bold())
        } else {
            Span::raw("  ")
        };
        Cell::from(Line::from(vec![
            marker,
            Span::styled(truncate(run_branch, 18), style),
        ]))
    };

    let rows: Vec<Row> = state
        .repos
        .iter()
        .map(|card| {
            // Same reasoning as the header above: until a repo is actually
            // entered, the workspace dashboard has no active repo to point at.
            let active = !state.repo_label_implicit && card.spec == state.repo_label;
            let name_style = if active {
                Style::default().fg(Color::White).bold()
            } else {
                Style::default().fg(Color::Rgb(200, 200, 220))
            };
            let name_cell = Cell::from(Line::from(vec![
                Span::styled(card.spec.clone(), name_style),
                if active {
                    Span::styled("  ●", Style::default().fg(theme.accent))
                } else {
                    Span::raw("")
                },
            ]));

            // A repo that failed to load says so instead of pretending to be idle.
            if let Some(err) = &card.error {
                return Row::new(vec![
                    Cell::from(Span::styled("!", Style::default().fg(theme.failure).bold())),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Span::styled(
                        truncate(err, 46),
                        Style::default().fg(theme.failure),
                    )),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ])
                .style(Style::default().bg(row_bg_for_status(Status::Failure)));
            }

            // A checkout with no GitHub origin never fetches, so it must not sit
            // at "loading…" forever — it just has no CI half to show.
            if !card.has_ci() {
                return Row::new(vec![
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Span::styled(
                        "local only — no GitHub remote",
                        Style::default().fg(Color::Rgb(90, 90, 115)).italic(),
                    )),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ]);
            }

            if !card.loaded {
                return Row::new(vec![
                    Cell::from(""),
                    name_cell,
                    local_cell(card),
                    changes_cell(card),
                    Cell::from(Span::styled("loading…", Style::default().fg(Color::DarkGray))),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ]);
            }

            let latest = card.runs.first();
            let status = card.latest_status().unwrap_or(Status::Unknown);
            let (when_text, when_style) = latest
                .map(|r| relative_styled(r.updated_at))
                .unwrap_or_else(|| ("—".into(), Style::default().fg(theme.unknown)));
            let workflow = latest
                .map(|r| truncate(&r.display_title, 28))
                .unwrap_or_else(|| "no runs".into());
            let branch = latest.map(|r| r.head_branch.clone()).unwrap_or_default();

            let (c_ok, c_fail, c_busy) = card.counts();
            let sparkline = Line::from(vec![
                Span::styled(format!("✓{c_ok} "), Style::default().fg(theme.success)),
                Span::styled(format!("✗{c_fail} "), Style::default().fg(theme.failure)),
                if c_busy > 0 {
                    Span::styled(format!("⏵{c_busy}"), Style::default().fg(theme.warning).bold())
                } else {
                    Span::raw("")
                },
            ]);

            Row::new(vec![
                Cell::from(Span::styled(
                    animated_glyph(status, state.tick_count),
                    style_for_status(status, theme),
                )),
                name_cell,
                local_cell(card),
                changes_cell(card),
                Cell::from(Span::styled(workflow, Style::default().fg(Color::Rgb(180, 180, 210)))),
                ran_on_cell(card, &branch),
                Cell::from(Span::styled(when_text, when_style)),
                Cell::from(sparkline),
            ])
            .style(Style::default().bg(row_bg_for_status(status)))
        })
        .collect();

    let widths = [
        Constraint::Length(1),   // status glyph
        Constraint::Fill(40),    // repo
        Constraint::Fill(26),    // local branch + upstream drift
        Constraint::Length(9),   // working-tree changes
        Constraint::Fill(35),    // latest workflow
        Constraint::Fill(26),    // branch the run used
        Constraint::Length(10),  // updated
        Constraint::Length(14),  // counts
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(35, 95, 120))
                .fg(Color::Rgb(220, 240, 255))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut ts = TableState::default();
    ts.select(Some(state.repo_cursor));
    f.render_stateful_widget(table, inner, &mut ts);
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
                (k(&km.open_browser), "open the repo's Actions page"),
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
    }
}

fn render_help_overlay(f: &mut Frame, area: Rect, state: &AppState) {
    if !state.show_help {
        return;
    }
    let theme = &state.theme;
    let current = help_section_for(state.view);
    let mut sections = help_sections(&state.keymap);
    // Float the current view's section to just below Global, so "what can I do
    // here?" is answered without scrolling.
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

    let mut lines: Vec<Line> = Vec::new();
    for (title, rows) in &sections {
        let is_current = *title == current;
        let title_style = if is_current {
            Style::default().fg(theme.accent).bold()
        } else {
            Style::default().fg(theme.primary).bold()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {title}"), title_style),
            if is_current {
                Span::styled("  ← you are here", Style::default().fg(theme.accent))
            } else {
                Span::raw("")
            },
        ]));
        for (key, desc) in rows {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("{key:>key_w$}"),
                    Style::default().fg(Color::White).bold(),
                ),
                Span::raw("   "),
                Span::styled(*desc, Style::default().fg(Color::Rgb(190, 190, 215))),
            ]));
        }
        lines.push(Line::default());
    }

    let total = lines.len() as u16;
    let dialog_w = (area.width * 78 / 100).max(46).min(area.width);
    let dialog_h = (total + 3).min(area.height.saturating_sub(2)).max(6);
    let x = area.x + area.width.saturating_sub(dialog_w) / 2;
    let y = area.y + area.height.saturating_sub(dialog_h) / 2;
    let popup = Rect { x, y, width: dialog_w, height: dialog_h };

    let inner_h = dialog_h.saturating_sub(2);
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = state.help_scroll.min(max_scroll);

    let hint = if max_scroll > 0 {
        format!(
            " jog v{}  —  Keys  ({}–{} of {})  {}/{} scroll · any key closes ",
            env!("CARGO_PKG_VERSION"),
            scroll + 1,
            (scroll + inner_h).min(total),
            total,
            display_key(&state.keymap.down),
            display_key(&state.keymap.up),
        )
    } else {
        format!(
            " jog v{}  —  Keys  ·  any key closes ",
            env!("CARGO_PKG_VERSION")
        )
    };

    let block = Block::default()
        .title(Span::styled(hint, Style::default().fg(theme.accent).bold()))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent));

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        popup,
    );
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
            Style::default().fg(Color::DarkGray),
        ))
    } else if gv.entries().is_empty() {
        Line::from(vec![
            Span::styled("✓ clean", Style::default().fg(theme.success).bold()),
            Span::styled(
                "   nothing to commit",
                Style::default().fg(theme.secondary),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!("{staged} staged"),
                if staged > 0 {
                    Style::default().fg(theme.success).bold()
                } else {
                    Style::default().fg(theme.secondary)
                },
            ),
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("{unstaged} unstaged"),
                Style::default().fg(theme.warning),
            ),
            Span::styled(
                format!("   {}", gv.path.display()),
                Style::default().fg(Color::Rgb(90, 90, 115)),
            ),
        ])
    };
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
                Style::default().fg(Color::Rgb(90, 90, 115))
            };
            let path_style = if staged {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            let label_style = match e.label() {
                "deleted" => Style::default().fg(theme.failure),
                "untracked" => Style::default().fg(Color::Rgb(120, 120, 145)),
                "conflict" => Style::default().fg(theme.failure).bold(),
                _ => Style::default().fg(theme.warning),
            };
            Row::new(vec![
                Cell::from(Span::styled(mark, mark_style)),
                Cell::from(Span::styled(e.code(), Style::default().fg(Color::Rgb(110, 110, 140)))),
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
            Cell::from(Span::styled("Change", Style::default().fg(theme.secondary))),
            Cell::from(Span::styled("File", Style::default().fg(theme.secondary))),
        ])
        .height(1)
        .bottom_margin(1),
    )
    .column_spacing(1)
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(35, 95, 120))
            .fg(Color::Rgb(220, 240, 255))
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
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let glyph = FRAMES[((state.tick_count / 2) % 10) as usize];
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
            Style::default().fg(Color::Rgb(110, 110, 140)),
        )));
    }
    for l in op.lines.iter().skip(offset).take(viewport - lines.len()) {
        let style = if l.error {
            Style::default().fg(theme.failure)
        } else if l.warn {
            Style::default().fg(theme.warning)
        } else {
            Style::default().fg(Color::Rgb(190, 190, 210))
        };
        lines.push(Line::from(Span::styled(l.text.clone(), style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "waiting for output…",
            Style::default().fg(Color::Rgb(110, 110, 140)),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Style one line of diff text.
///
/// Split out so the classification is testable, and because the `+++`/`---`
/// file headers are the easy thing to get wrong: they start with the same
/// characters as content but are not additions or deletions.
fn diff_line_style(text: &str, theme: &Theme) -> Style {
    let meta = Style::default().fg(Color::Rgb(110, 110, 140));
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
        _ => Style::default().fg(Color::Rgb(190, 190, 210)),
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
            Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray).italic()))
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
            Span::styled(buf.as_str(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(accent)),
        ]),
        Line::from(Span::styled(
            "  ↵ commit · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(Paragraph::new(lines), inner);
}

/// Cut `s` to `max` characters, adding an ellipsis when it was longer.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    format!("{}…", chars[..max.saturating_sub(1)].iter().collect::<String>())
}

fn render_finder_overlay(f: &mut Frame, area: Rect, state: &AppState) {
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
            Span::styled(finder.query.as_str(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(accent)),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(Color::Rgb(55, 55, 80)),
        )),
    ];

    if finder.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(Color::DarkGray).italic(),
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
            Style::default().fg(Color::Rgb(220, 240, 255)).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        let line = Line::from(vec![
            Span::styled(if selected { "▶ " } else { "  " }, Style::default().fg(accent)),
            Span::styled(truncate(label, inner.width.saturating_sub(3) as usize), style),
        ]);
        lines.push(if selected {
            line.style(Style::default().bg(Color::Rgb(35, 95, 120)))
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
    let first = (state.log_scroll as usize).min(state.log_rendered.len());
    // Every rendered line occupies at least one screen row, so `viewport` of
    // them is all the widget can possibly draw. Cutting there keeps the per-frame
    // clone bounded instead of copying the tail of a 40k-line log each redraw.
    // Cursor and current-match highlighting are applied here rather than during
    // the rebuild, so moving the cursor costs a viewport, not the whole buffer.
    let p = Paragraph::new(state.decorate_visible(first, viewport as usize))
        .block(styled_block(&log_title, &state.theme))
        .wrap(Wrap { trim: false });
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

    #[test]
    fn a_failed_hook_shows_the_failure_not_just_that_it_failed() {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in [
            "ruff.....Passed",
            "pytest...Failed",
            "FAILED tests/test_api.py::test_login",
            "assert 1 == 2",
        ] {
            op.push_line(l.into());
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
            op.push_line(format!("collecting test {i}"));
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
    #[ignore = "visual check: cargo test show_changes -- --ignored --nocapture"]
    fn show_changes() {
        let mut running = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in ["ruff.....Passed", "pyright..", "collecting tests…"] {
            running.push_line(l.into());
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
            failed.push_line(l.into());
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
        let meta = Color::Rgb(110, 110, 140);
        assert_eq!(fg("+++ b/src/main.rs"), meta);
        assert_eq!(fg("--- a/src/main.rs"), meta);
        assert_eq!(fg("diff --git a/x b/x"), meta);
        assert_eq!(fg("Binary files a/q.png and b/q.png differ"), meta);

        assert_eq!(fg("+let x = 1;"), theme.success);
        assert_eq!(fg("-let x = 0;"), theme.failure);
        assert_eq!(fg("@@ -1,4 +1,4 @@"), theme.accent);
        // A context line keeps the neutral body colour, blank lines included.
        assert_eq!(fg(" unchanged"), Color::Rgb(190, 190, 210));
        assert_eq!(fg(""), Color::Rgb(190, 190, 210));
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
