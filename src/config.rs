use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub keys: KeymapConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    pub repo: Option<String>,
}

fn default_provider_kind() -> String {
    "github".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_poll_ms")]
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub favorites: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            poll_interval_ms: default_poll_ms(),
            favorites: Vec::new(),
        }
    }
}

fn default_theme() -> String {
    "dark".into()
}
fn default_poll_ms() -> u64 {
    5000
}

/// Key name strings exactly as they appear in config.toml [keys].
/// Single chars ("j", "R"), special names ("Enter", "Esc", "Space",
/// "PageUp", "PageDown", "Up", "Down"), or modifier combos ("ctrl+c").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeymapConfig {
    // global
    pub quit: String,
    pub back: String,
    // navigation (shared across list views and scroll)
    pub down: String,
    pub up: String,
    // confirm / open
    pub confirm: String,
    pub open_logs: String,
    // log scrolling
    pub page_down: String,
    pub page_up: String,
    pub scroll_top: String,
    // log step navigation
    pub next_step: String,
    pub prev_step: String,
    pub all_steps: String,
    // log search (`n`/`p` reused for next/prev match while a query is active)
    pub search: String,
    // workflow actions
    pub trigger: String,
    pub watch: String,
    pub open_browser: String,
    // run actions
    pub cancel_run: String,
    pub rerun: String,
    pub rerun_failed: String,
    pub diff: String,
    // trigger-prompt (normal mode)
    pub tp_edit: String,
    pub tp_submit: String,
    pub tp_yes: String,
    pub tp_no: String,
    pub tp_cycle: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            quit: "q".into(),
            back: "Esc".into(),
            down: "j".into(),
            up: "k".into(),
            confirm: "Enter".into(),
            open_logs: "l".into(),
            page_down: "d".into(),
            page_up: "u".into(),
            scroll_top: "g".into(),
            next_step: "n".into(),
            prev_step: "p".into(),
            all_steps: "a".into(),
            search: "/".into(),
            trigger: "t".into(),
            watch: "w".into(),
            open_browser: "o".into(),
            cancel_run: "x".into(),
            rerun: "r".into(),
            rerun_failed: "R".into(),
            diff: "D".into(),
            tp_edit: "i".into(),
            tp_submit: "t".into(),
            tp_yes: "y".into(),
            tp_no: "n".into(),
            tp_cycle: "Space".into(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn path() -> PathBuf {
        if let Some(dir) = dirs::config_dir() {
            return dir.join("jog").join("config.toml");
        }
        Path::new(".").join("jog.toml")
    }
}
