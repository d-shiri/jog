use anyhow::{Context, Result, anyhow};
use clap::Parser;
use std::sync::Arc;

mod app;
mod cli;
mod git;
mod config;
mod history;
mod kuma;
mod provider;
mod tui;

use crate::app::state::View;
use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::provider::Provider;
use crate::provider::discovery::{discover_workflows, find_repo_root};
use crate::provider::github::{GitHubProvider, RepoSpec, current_branch, resolve_token};
use crate::tui::TuiOpts;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load().context("load config")?;

    let cwd = std::env::current_dir().context("cwd")?;

    // Sitting inside a checkout is the normal case. Outside one — a directory
    // whose *children* are repos — fall back to scanning for them so the tool
    // still has something to show.
    let repo_root = find_repo_root(&cwd).ok();
    let workspace = match &repo_root {
        Some(_) => Vec::new(),
        None => git::discover_workspace(&cwd),
    };

    let workflows = match &repo_root {
        Some(root) => discover_workflows(root)?,
        None => Vec::new(),
    };

    let repo_spec = match cli.repo.clone().or(config.provider.repo.clone()) {
        Some(s) => Some(RepoSpec::parse(&s)?),
        None if repo_root.is_some() => Some(RepoSpec::from_git_remote()?),
        // In workspace mode the "active" repo is just whichever discovered repo
        // has a usable GitHub remote — the dashboard swaps it as you navigate.
        None => workspace.iter().find_map(|p| {
            let url = git::remote_url(p).ok()?;
            crate::provider::github::parse_remote_url(&url).ok()
        }),
    };

    // An explicit `--repo` (or `[provider] repo`) is the escape hatch the error
    // below advertises, so it has to actually work: with one named repo there is
    // nothing to discover and no reason to insist on a checkout.
    if repo_root.is_none() && workspace.is_empty() && repo_spec.is_none() {
        return Err(anyhow!(
            "not inside a git repository, and no repositories found under {}\n  \
             fix: cd into a checkout, pass --repo owner/name, or run from a directory whose subfolders are repos",
            cwd.display()
        ));
    }

    // A workspace whose repos have no GitHub remotes still has a working
    // dashboard and commit flow — only the CI half is unavailable. Don't make
    // that a hard failure, and don't demand credentials we'll never spend.
    let has_remote = repo_spec.is_some();
    let repo_spec = repo_spec.unwrap_or_else(|| RepoSpec {
        owner: String::new(),
        repo: String::new(),
    });
    let token = if has_remote {
        resolve_token()?
    } else {
        resolve_token().unwrap_or_default()
    };
    let provider = Arc::new(GitHubProvider::new(repo_spec, token)?);

    // Outside a checkout there are no local workflows to list, so the dashboard
    // is the only sensible landing view.
    let default_view = if repo_root.is_some() {
        View::Workflows
    } else {
        View::Repos
    };
    // Only a real workspace scan gets its root shown in the header; with an
    // explicit `--repo` outside a checkout the cwd is meaningless.
    let workspace_root = (!workspace.is_empty()).then(|| cwd.clone());

    match cli.command {
        None => {
            tui::run(
                provider,
                workflows,
                config,
                TuiOpts {
                    initial_view: default_view,
                    focus_workflow: None,
                    workspace,
                    workspace_root,
                },
            )
            .await
        }
        Some(Command::Run {
            workflow,
            reference,
            inputs,
        }) => {
            let wf = resolve_workflow_full(&workflows, &workflow)?;
            let r = if reference.is_empty() {
                current_branch().unwrap_or_else(|_| "main".into())
            } else {
                reference
            };
            let user_inputs = parse_kv(&inputs)?;
            let merged = wf.merge_defaults(user_inputs);
            let still_missing: Vec<&str> = wf
                .missing_required_inputs()
                .into_iter()
                .filter(|k| !merged.contains_key(*k))
                .collect();
            if !still_missing.is_empty() {
                return Err(anyhow!(
                    "{} requires inputs: {} (use --input KEY=VAL)",
                    wf.file_name,
                    still_missing.join(", ")
                ));
            }
            provider
                .trigger(&wf.file_name, &r, merged)
                .await
                .with_context(|| format!("trigger {} on {}", wf.file_name, r))?;
            println!("triggered {} on {}", wf.file_name, r);
            Ok(())
        }
        Some(Command::Watch { workflow }) => {
            let resolved = resolve_workflow(&workflows, &workflow)?;
            tui::run(
                provider,
                workflows,
                config,
                TuiOpts {
                    initial_view: View::Watch,
                    focus_workflow: Some(resolved),
                    workspace: Vec::new(),
                    workspace_root: None,
                },
            )
            .await
        }
        Some(Command::Repos) => {
            tui::run(
                provider,
                workflows,
                config,
                TuiOpts {
                    initial_view: View::Repos,
                    focus_workflow: None,
                    workspace,
                    workspace_root,
                },
            )
            .await
        }
        Some(Command::Open { workflow }) => {
            let resolved = resolve_workflow(&workflows, &workflow)?;
            let latest = provider
                .get_latest_run(&resolved)
                .await?
                .ok_or_else(|| anyhow!("no runs for {}", resolved))?;
            open::that(&latest.url).context("open browser")?;
            println!("opened {}", latest.url);
            Ok(())
        }
    }
}

fn resolve_workflow(
    workflows: &[crate::provider::Workflow],
    query: &str,
) -> Result<String> {
    resolve_workflow_full(workflows, query).map(|w| w.file_name.clone())
}

fn resolve_workflow_full<'a>(
    workflows: &'a [crate::provider::Workflow],
    query: &str,
) -> Result<&'a crate::provider::Workflow> {
    if let Some(w) = workflows.iter().find(|w| w.file_name == query) {
        return Ok(w);
    }
    let lower = query.to_lowercase();
    let candidates: Vec<&crate::provider::Workflow> = workflows
        .iter()
        .filter(|w| {
            w.name.to_lowercase().contains(&lower) || w.file_name.to_lowercase().contains(&lower)
        })
        .collect();
    match candidates.len() {
        0 => Err(anyhow!("no workflow matches `{query}`")),
        1 => Ok(candidates[0]),
        _ => Err(anyhow!(
            "multiple workflows match `{query}`: {}",
            candidates
                .iter()
                .map(|w| w.file_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn parse_kv(raw: &[String]) -> Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    for s in raw {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| anyhow!("--input expects KEY=VAL, got `{s}`"))?;
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}
