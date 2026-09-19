use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A checkout's own settings, read from the root of the repo `jog` is launched
/// in and laid over the global config.
///
/// The global file is where a machine says how it likes `jog` to behave; this
/// is where a *project* says what `jog` should be pointed at while you are in
/// it — which is almost always its own status page, and never the last
/// project's. Every key of the global file is overridable, section by section:
/// what the local file leaves out, the global one still answers.
pub const LOCAL_CONFIG_NAME: &str = ".jog.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub keys: KeymapConfig,
    /// Service health from an Uptime Kuma status page. Absent = the feature
    /// does not exist: no column, no tally, no requests.
    #[serde(default, alias = "kuma")]
    pub uptime_kuma: Option<UptimeKumaConfig>,
}

/// `[uptime_kuma]` — the two lines that turn service health on:
///
/// ```toml
/// [uptime_kuma]
/// url = "https://up.example.com"
/// # status_page = "default"          # the page's slug, if not the default
/// # [uptime_kuma.map]                # monitor name -> dashboard repo, for
/// # "API" = "acme/backend"           # names that don't match a repo's name
/// ```
///
/// Monitors whose name matches a repo's name (case-insensitive, short or
/// full) attach themselves to that row with no map at all; everything else
/// still counts in the header's health tally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UptimeKumaConfig {
    /// Base URL of the Kuma instance, e.g. `https://up.example.com`.
    pub url: String,
    /// Slug of the status page to read. Kuma names its first one `default`.
    #[serde(default = "default_status_page", alias = "slug")]
    pub status_page: String,
    /// Monitor name → repo (`owner/name`), for monitors whose name says
    /// nothing about which repo they belong to.
    #[serde(default)]
    pub map: std::collections::HashMap<String, String>,
    /// Seconds between reads of the status page. Its own clock, slower than
    /// the CI poll: monitors check on the order of minutes, so re-fetching
    /// every few seconds re-downloads an answer that hasn't moved.
    #[serde(default = "default_kuma_poll_s")]
    pub poll_interval_s: u64,
}

fn default_status_page() -> String {
    "default".into()
}

fn default_kuma_poll_s() -> u64 {
    30
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
    /// Sound file played when jog puts a question on screen and waits — the
    /// push prompt after a commit lands. Empty means use the bundled chime.
    #[serde(default)]
    pub ask_sound: String,
    /// Which finished runs to announce: `always`, `failure`, or `never`.
    #[serde(default = "default_notify")]
    pub notify: String,
    /// Play a sound when a run is announced.
    #[serde(default = "default_true")]
    pub notify_sound: bool,
    /// Play a sound when a question appears and waits for an answer. Separate
    /// from `notify_sound`: a run finishing is news you can read later, a
    /// question is a keystroke jog is holding still for, and someone who wants
    /// the second without the first should be able to say so.
    #[serde(default = "default_true")]
    pub ask_sound_enabled: bool,
    /// Raise an OS desktop notification when a run is announced.
    #[serde(default = "default_true")]
    pub notify_desktop: bool,
    /// The header's bell while notifications are live, and its slashed twin
    /// while they are snoozed.
    ///
    /// Unset uses Nerd Font bells, same convention as `github_icon`: a
    /// terminal without a patched font draws boxes, so set your own strings
    /// there (`"on"` / `"off"`, `"🔔"` / `"🔕"`), or `""` to hide the bell —
    /// a snooze then still shows as text, because a mute nothing admits to
    /// is how a failure goes unheard.
    #[serde(default)]
    pub bell_icon: Option<String>,
    #[serde(default)]
    pub bell_off_icon: Option<String>,
    /// Language marks on the file bands of the combined diff — a Rust gear on
    /// `.rs`, a snake on `.py`, and so on.
    ///
    /// Nerd Font glyphs, same convention as `github_icon`: a terminal without
    /// a patched font draws them as boxes, so set this to `false` to turn
    /// them off.
    #[serde(default = "default_true")]
    pub file_icons: bool,
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
            ask_sound: String::new(),
            github_icon: None,
            notify: default_notify(),
            notify_sound: true,
            ask_sound_enabled: true,
            notify_desktop: true,
            bell_icon: None,
            bell_off_icon: None,
            file_icons: true,
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
    /// Fetch whatever the current screen shows, again. Global: it means the
    /// same thing on every view, which is the whole point of it. `git_refresh`
    /// is the name it had while it only ever re-read a working tree.
    #[serde(alias = "git_refresh")]
    pub refresh: String,
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
    // service-health overlay (Uptime Kuma monitors by name)
    pub services: String,
    // snooze notifications: 30m, press again for 60m, again for off
    pub snooze: String,
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
            refresh: "r".into(),
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
            services: "S".into(),
            snooze: "Z".into(),
            repo_mark: "Space".into(),
            batch_commit: "C".into(),
            batch_retry: "t".into(),
            batch_skip: "s".into(),
            git_view: "c".into(),
            git_stage: "Space".into(),
            git_stage_all: "a".into(),
            git_commit: "c".into(),
            git_push: "P".into(),
            git_diff: "d".into(),
            trigger: "t".into(),
            watch: "w".into(),
            open_browser: "o".into(),
            cancel_run: "x".into(),
            rerun: "ctrl+r".into(),
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
    /// The global config with `repo_root`'s own [`LOCAL_CONFIG_NAME`] laid over
    /// it, key by key.
    ///
    /// Merging rather than replacing is what makes the local file worth having:
    /// a project that only wants its own status page writes two lines and keeps
    /// the theme, the keymap and the sounds its owner configured once.
    pub fn load_for(repo_root: Option<&Path>) -> Result<Self> {
        let global = read_table(&Self::path())?;
        let local = match repo_root.map(|r| Self::local_path(r)) {
            Some(path) => read_table(&path)?,
            None => toml::Table::new(),
        };
        Self::layer(global, local)
    }

    /// The merge itself, given both files already parsed — the whole of
    /// [`load_for`](Self::load_for) that doesn't touch the disk.
    fn layer(mut global: toml::Table, mut local: toml::Table) -> Result<Self> {
        canonicalize_aliases(&mut global);
        canonicalize_aliases(&mut local);
        merge_tables(&mut global, local);
        let mut cfg: Self = toml::Value::Table(global)
            .try_into()
            .context("parse config")?;
        // `url = ""` is how a project says *no* status page while the global
        // config has one. Without it the local file could only ever point the
        // feature somewhere else, never turn it off.
        if cfg
            .uptime_kuma
            .as_ref()
            .is_some_and(|k| k.url.trim().is_empty())
        {
            cfg.uptime_kuma = None;
        }
        Ok(cfg)
    }

    /// Where a checkout keeps its own settings.
    pub fn local_path(repo_root: &Path) -> PathBuf {
        repo_root.join(LOCAL_CONFIG_NAME)
    }

    pub fn path() -> PathBuf {
        if let Some(dir) = dirs::config_dir() {
            return dir.join("jog").join("config.toml");
        }
        Path::new(".").join("jog.toml")
    }
}

/// A config file as a raw table, or an empty one when it isn't there.
///
/// Absent is not an error — neither file is required — but unreadable and
/// malformed are: a config that silently does nothing is indistinguishable
/// from a broken one.
fn read_table(path: &Path) -> Result<toml::Table> {
    if !path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read config {}", path.display()))?;
    raw.parse::<toml::Table>()
        .with_context(|| format!("parse config {}", path.display()))
}

/// Fold the spellings serde accepts as aliases into the names it stores them
/// under, before the two files are merged.
///
/// [`merge_tables`] matches on the raw TOML key and knows nothing about serde,
/// so a global file written with the old name and a local one written with the
/// new name arrive as two unrelated keys and *both* survive into the merged
/// table. serde then refuses the duplicate — `duplicate field \`refresh\`` —
/// and since `main` propagates that, jog does not start at all in that repo.
/// The README hands out exactly this trap: `refresh = "r"  # was \`git_refresh\`;
/// the old name still works`. Folding the aliases first is what makes the two
/// layers talk about the same field.
fn canonicalize_aliases(t: &mut toml::Table) {
    // The canonical spelling wins when one file carries both: it is the name
    // the current docs tell you to write.
    fn rename(t: &mut toml::Table, from: &str, to: &str) {
        if let Some(v) = t.remove(from)
            && !t.contains_key(to)
        {
            t.insert(to.to_string(), v);
        }
    }
    rename(t, "kuma", "uptime_kuma");
    if let Some(toml::Value::Table(k)) = t.get_mut("uptime_kuma") {
        rename(k, "slug", "status_page");
    }
    if let Some(toml::Value::Table(k)) = t.get_mut("keys") {
        rename(k, "git_refresh", "refresh");
    }
}

/// Lay `over` on top of `base`, recursing into tables.
///
/// Sub-tables merge so a local `[ui] theme = …` doesn't erase the global
/// `[ui.colors]`; everything else — scalars and arrays alike — is replaced
/// whole, because a half-overridden list of favourites is nobody's intent.
fn merge_tables(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_tables(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(raw: &str) -> toml::Table {
        raw.parse().expect("test fixture parses")
    }

    /// An old spelling in one layer and the new one in the other used to reach
    /// serde as two separate keys, and `duplicate field` is a hard error: jog
    /// refused to start in that repo at all.
    #[test]
    fn the_two_layers_may_spell_a_field_differently() {
        let cfg = Config::layer(
            table("[keys]\ngit_refresh = \"r\"\n"),
            table("[keys]\nrefresh = \"R\"\n"),
        )
        .expect("old name globally, new name locally");
        assert_eq!(cfg.keys.refresh, "R", "the local file still wins");

        let cfg = Config::layer(
            table("[uptime_kuma]\nurl = \"https://up.example.com\"\n"),
            table("[kuma]\nurl = \"https://local.example.com\"\n"),
        )
        .expect("section named both ways");
        assert_eq!(
            cfg.uptime_kuma.expect("kept").url,
            "https://local.example.com"
        );

        let cfg = Config::layer(
            table("[uptime_kuma]\nurl = \"https://up.example.com\"\nslug = \"one\"\n"),
            table("[uptime_kuma]\nstatus_page = \"two\"\n"),
        )
        .expect("field named both ways");
        assert_eq!(cfg.uptime_kuma.expect("kept").status_page, "two");
    }

    /// One file carrying both spellings is a question the docs answer: the
    /// current name is the one to write, so it is the one that counts.
    #[test]
    fn the_current_spelling_wins_inside_one_file() {
        let cfg = Config::layer(
            table("[keys]\nrefresh = \"R\"\ngit_refresh = \"r\"\n"),
            toml::Table::new(),
        )
        .expect("both spellings in one file");
        assert_eq!(cfg.keys.refresh, "R");
    }

    const GLOBAL: &str = r##"
        [ui]
        theme = "midnight"
        [ui.colors]
        accent = "#ff0000"
        [uptime_kuma]
        url = "https://up.example.com"
        status_page = "all"
        [uptime_kuma.map]
        "API" = "acme/backend"
    "##;

    #[test]
    fn a_repo_points_service_health_at_its_own_page() {
        let cfg = Config::layer(
            table(GLOBAL),
            table(r#"[uptime_kuma]
                     url = "https://status.other.dev"
                     status_page = "public""#),
        )
        .unwrap();
        let k = cfg.uptime_kuma.expect("still configured");
        assert_eq!(k.url, "https://status.other.dev");
        assert_eq!(k.status_page, "public");
        // Untouched by the local file, so the global answer stands.
        assert_eq!(cfg.ui.theme, "midnight");
    }

    #[test]
    fn what_the_local_file_leaves_out_the_global_still_answers() {
        // Only the slug moves: the instance URL and the monitor map are the
        // machine's settings and have no business being retyped per project.
        let cfg = Config::layer(
            table(GLOBAL),
            table(r#"[uptime_kuma]
                     status_page = "checkout""#),
        )
        .unwrap();
        let k = cfg.uptime_kuma.unwrap();
        assert_eq!(k.url, "https://up.example.com");
        assert_eq!(k.status_page, "checkout");
        assert_eq!(k.map.get("API").map(String::as_str), Some("acme/backend"));
    }

    #[test]
    fn an_empty_url_turns_service_health_off_for_this_repo() {
        let cfg = Config::layer(table(GLOBAL), table(r#"[uptime_kuma]
                                                        url = """#))
            .unwrap();
        assert!(cfg.uptime_kuma.is_none(), "no column, no tally, no requests");
    }

    #[test]
    fn sub_tables_merge_rather_than_replace_each_other() {
        let cfg = Config::layer(
            table(GLOBAL),
            table(r#"[ui]
                     theme = "paper""#),
        )
        .unwrap();
        assert_eq!(cfg.ui.theme, "paper");
        // Setting the theme locally must not erase the global colour overrides.
        assert_eq!(cfg.ui.colors.get("accent").map(String::as_str), Some("#ff0000"));
    }

    #[test]
    fn a_repo_can_name_the_others_it_wants_beside_it() {
        let cfg = Config::layer(
            table(r#"[provider]
                     repos = ["acme/one"]"#),
            table(r#"[provider]
                     repos = ["acme/two", "acme/three"]"#),
        )
        .unwrap();
        // Arrays replace whole: a half-overridden list is nobody's intent.
        assert_eq!(cfg.provider.repos, vec!["acme/two", "acme/three"]);
    }

    #[test]
    fn no_local_file_is_the_global_config_unchanged() {
        let cfg = Config::layer(table(GLOBAL), toml::Table::new()).unwrap();
        assert_eq!(cfg.uptime_kuma.unwrap().url, "https://up.example.com");
    }
}
