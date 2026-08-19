use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_yml::Value;
use std::path::{Path, PathBuf};

use super::{Workflow, WorkflowInput};

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut current = start
        .canonicalize()
        .with_context(|| format!("canonicalize {}", start.display()))?;
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return Err(anyhow!("not inside a git repository")),
        }
    }
}

pub fn discover_workflows(repo_root: &Path) -> Result<Vec<Workflow>> {
    let workflows_dir = repo_root.join(".github").join("workflows");
    if !workflows_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&workflows_dir)
        .with_context(|| format!("read {}", workflows_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "yml" || e == "yaml")
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        match parse_workflow(&path) {
            Ok(wf) => out.push(wf),
            Err(_) => continue,
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct WorkflowYaml {
    name: Option<String>,
    on: Option<Value>,
}

fn parse_workflow(path: &Path) -> Result<Workflow> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    parse_workflow_str(&raw, &file_name)
}

/// Parse workflow YAML that is already in memory. Used both for files on disk
/// and for workflow contents fetched from the API for repos we have no checkout of.
pub fn parse_workflow_str(raw: &str, file_name: &str) -> Result<Workflow> {
    let parsed: WorkflowYaml =
        serde_yml::from_str(raw).with_context(|| format!("parse YAML {file_name}"))?;
    let name =
        super::emoji_width_safe(&parsed.name.unwrap_or_else(|| file_name.to_string()));
    let triggerable = has_workflow_dispatch(parsed.on.as_ref());
    let inputs = parse_inputs(parsed.on.as_ref());
    Ok(Workflow {
        name,
        file_name: file_name.to_string(),
        triggerable,
        last_status: None,
        last_run_at: None,
        inputs,
    })
}

fn parse_inputs(on: Option<&Value>) -> Vec<WorkflowInput> {
    let Some(Value::Mapping(map)) = on else {
        return Vec::new();
    };
    let dispatch = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("workflow_dispatch"))
        .map(|(_, v)| v);
    let Some(Value::Mapping(dispatch)) = dispatch else {
        return Vec::new();
    };
    let inputs = dispatch
        .iter()
        .find(|(k, _)| k.as_str() == Some("inputs"))
        .map(|(_, v)| v);
    let Some(Value::Mapping(inputs)) = inputs else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in inputs {
        let Some(name) = k.as_str() else { continue };
        let (required, default, options) = match v {
            Value::Mapping(m) => {
                let req = m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("required"))
                    .and_then(|(_, v)| v.as_bool())
                    .unwrap_or(false);
                let def = m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("default"))
                    .map(|(_, v)| value_to_string(v));
                let opts = m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("options"))
                    .and_then(|(_, v)| match v {
                        Value::Sequence(items) => Some(
                            items
                                .iter()
                                .map(value_to_string)
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .filter(|v: &Vec<String>| !v.is_empty());
                (req, def, opts)
            }
            _ => (false, None, None),
        };
        out.push(WorkflowInput {
            name: name.to_string(),
            required,
            default,
            options,
        });
    }
    out
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn has_workflow_dispatch(on: Option<&Value>) -> bool {
    let Some(on) = on else { return false };
    match on {
        Value::String(s) => s == "workflow_dispatch",
        Value::Sequence(items) => items.iter().any(|v| {
            v.as_str()
                .map(|s| s == "workflow_dispatch")
                .unwrap_or(false)
        }),
        Value::Mapping(map) => map.keys().any(|k| {
            k.as_str()
                .map(|s| s == "workflow_dispatch")
                .unwrap_or(false)
        }),
        _ => false,
    }
}
