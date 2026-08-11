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
    /// Extra repos (`owner/name`) shown side by side in the multi-repo dashboard.
    /// The active repo is always included, so listing it here is optional.
    #[serde(default)]
    pub repos: Vec<String>,
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
    /// Sound file played when a watched run finishes successfully.
    /// Empty means use the bundled finished sound.
    #[serde(default)]
    pub complete_sound: String,
    /// Sound file played when a watched run finishes with a failure.
    /// Empty means use the bundled fail sound.
    #[serde(default)]
    pub fail_sound: String,
    /// Glyph drawn to the left of every repo that lives on GitHub.
    ///
    /// Unset uses the bundled Nerd Font mark; set it to `""` to turn the icons
    /// off, or to any string of your own — a terminal without a patched font
    /// draws the default as an empty box.
    #[serde(default)]
    pub github_icon: Option<String>,
    /// Sound file played when the GitHub API budget is nearly spent.
    /// Empty means use the bundled quota alarm.
    #[serde(default)]
    pub quota_sound: String,
    /// Which finished runs to announce: `always`, `failure`, or `never`.
    #[serde(default = "default_notify")]
    pub notify: String,
    /// Play a sound when a run is announced.
    #[serde(default = "default_true")]
    pub notify_sound: bool,
    /// Raise an OS desktop notification when a run is announced.
    #[serde(default = "default_true")]
    pub notify_desktop: bool,
    /// Lines of surrounding context kept around each error/warning in log focus mode.
    #[serde(default = "default_focus_context")]
    pub log_focus_context: usize,
    /// Per-token colour overrides on top of `theme`, as `token = "#rrggbb"`.
    ///
    /// A whole palette is the blunt instrument; this is for the one colour that
    /// is wrong on your terminal. Unknown token names are reported rather than
    /// ignored, since a silent typo looks exactly like the setting not working.
    #[serde(default)]
    pub colors: std::collections::HashMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            poll_interval_ms: default_poll_ms(),
            favorites: Vec::new(),
            complete_sound: String::new(),
            fail_sound: String::new(),
            quota_sound: String::new(),
            github_icon: None,
            notify: default_notify(),
            notify_sound: true,
            notify_desktop: true,
            log_focus_context: default_focus_context(),
            colors: std::collections::HashMap::new(),
        }
    }
}

/// When a finished run should be announced (sound + desktop notification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Always,
    Failure,
    Never,
}

impl UiConfig {
    /// Parse `ui.notify`. Unknown values fall back to `Always` rather than
    /// failing the whole config — a typo shouldn't stop `jog` from starting.
    pub fn notify_mode(&self) -> NotifyMode {
        match self.notify.trim().to_lowercase().as_str() {
            "never" | "off" | "none" => NotifyMode::Never,
            "failure" | "failures" | "fail" => NotifyMode::Failure,
            _ => NotifyMode::Always,
        }
    }
}

fn default_theme() -> String {
    "midnight".into()
}
fn default_poll_ms() -> u64 {
    5000
}
fn default_notify() -> String {
    "always".into()
}
fn default_true() -> bool {
    true
}
fn default_focus_context() -> usize {
    2
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
    pub help: String,
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
    pub scroll_bottom: String,
    // log step navigation
    pub next_step: String,
    pub prev_step: String,
    pub all_steps: String,
    // log search (`n`/`p` reused for next/prev match while a query is active)
    pub search: String,
    // log focus mode + error jumping
    pub log_focus: String,
    pub next_error: String,
    pub prev_error: String,
    // fuzzy finder over the current list
    pub finder: String,
    // multi-repo dashboard
    pub repos_view: String,
    // batch commit: mark repos on the dashboard, commit them with one message
    pub repo_mark: String,
    pub batch_commit: String,
    pub batch_retry: String,
    pub batch_skip: String,
    // working tree (stage / commit / push) for a local checkout
    pub git_view: String,
    pub git_stage: String,
    pub git_stage_all: String,
    pub git_commit: String,
    pub git_push: String,
    pub git_refresh: String,
    pub git_diff: String,
    // workflow actions
    pub trigger: String,
    pub watch: String,
    pub open_browser: String,
    // run actions
    pub cancel_run: String,
    pub rerun: String,
    pub rerun_failed: String,
    pub diff: String,
    pub yank: String,
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
            help: "?".into(),
            down: "j".into(),
            up: "k".into(),
            confirm: "Enter".into(),
            open_logs: "l".into(),
            page_down: "d".into(),
            page_up: "u".into(),
            scroll_top: "g".into(),
            scroll_bottom: "G".into(),
            next_step: "n".into(),
            prev_step: "p".into(),
            all_steps: "a".into(),
            search: "/".into(),
            log_focus: "F".into(),
            next_error: "e".into(),
            prev_error: "E".into(),
            finder: "ctrl+p".into(),
            repos_view: "H".into(),
            repo_mark: "Space".into(),
            batch_commit: "C".into(),
            batch_retry: "r".into(),
            batch_skip: "s".into(),
            git_view: "c".into(),
            git_stage: "Space".into(),
            git_stage_all: "a".into(),
            git_commit: "c".into(),
            git_push: "P".into(),
            git_refresh: "r".into(),
            git_diff: "d".into(),
            trigger: "t".into(),
            watch: "w".into(),
            open_browser: "o".into(),
            cancel_run: "x".into(),
            rerun: "r".into(),
            rerun_failed: "R".into(),
            diff: "D".into(),
            yank: "y".into(),
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
