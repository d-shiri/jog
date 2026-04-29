use anyhow::{Context, Result, anyhow};
use clap::Parser;
use std::sync::Arc;

mod app;
mod cli;
mod config;
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
    let repo_root = find_repo_root(&cwd).context("find repo root")?;
    let workflows = discover_workflows(&repo_root)?;

    let repo_spec = match cli.repo.clone().or(config.provider.repo.clone()) {
        Some(s) => RepoSpec::parse(&s)?,
        None => RepoSpec::from_git_remote()?,
    };

    let token = resolve_token()?;
    let provider = Arc::new(GitHubProvider::new(repo_spec, token)?);

    match cli.command {
        None => {
            tui::run(
                provider,
                workflows,
                config,
                TuiOpts {
                    initial_view: View::Workflows,
                    focus_workflow: None,
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
