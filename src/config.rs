use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub ui: UiConfig,
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
