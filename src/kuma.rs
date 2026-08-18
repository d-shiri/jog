//! Service health from an Uptime Kuma status page.
//!
//! The dashboard already answers "did CI pass?"; this is the other half of
//! the question — "and is the thing it deployed actually up?". Kuma publishes
//! any status page as two unauthenticated JSON endpoints:
//!
//!   `/api/status-page/{slug}`            — the monitors, grouped, with names
//!   `/api/status-page/heartbeat/{slug}`  — recent beats + 24h uptime per id
//!
//! Both are fetched once per poll, off-thread, and joined by monitor id into
//! the flat list the header tally and the dashboard's Live column read. No
//! credentials are involved anywhere: whatever the status page shows the
//! world is exactly what jog sees.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;

/// What one monitor's newest heartbeat says. Kuma's own status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Up,
    Down,
    /// First checks after creation, or a retry in progress.
    Pending,
    Maintenance,
}

impl ServiceState {
    fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Up,
            2 => Self::Pending,
            3 => Self::Maintenance,
            _ => Self::Down,
        }
    }
}

/// One monitor, reduced to what a table cell and a tally need.
#[derive(Debug, Clone)]
pub struct Service {
    pub name: String,
    pub state: ServiceState,
    /// Latest response time, when the beat carried one.
    pub ping_ms: Option<u32>,
    /// Share of the last 24h the service was up, 0.0–1.0.
    pub uptime24: Option<f64>,
}

/// The two status-page documents, joined by monitor id.
///
/// Separate from `fetch` so the joining logic is testable against captured
/// JSON without a Kuma to talk to.
pub fn parse(page_json: &str, heartbeat_json: &str) -> Result<Vec<Service>> {
    #[derive(serde::Deserialize)]
    struct Page {
        #[serde(rename = "publicGroupList", default)]
        groups: Vec<Group>,
    }
    #[derive(serde::Deserialize)]
    struct Group {
        #[serde(rename = "monitorList", default)]
        monitors: Vec<Monitor>,
    }
    #[derive(serde::Deserialize)]
    struct Monitor {
        id: i64,
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct Heartbeats {
        #[serde(rename = "heartbeatList", default)]
        beats: HashMap<String, Vec<Beat>>,
        #[serde(rename = "uptimeList", default)]
        uptime: HashMap<String, f64>,
    }
    #[derive(serde::Deserialize)]
    struct Beat {
        status: i64,
        ping: Option<f64>,
    }

    let page: Page = serde_json::from_str(page_json).context("parse status page")?;
    let hb: Heartbeats = serde_json::from_str(heartbeat_json).context("parse heartbeats")?;

    let mut out = Vec::new();
    for group in page.groups {
        for m in group.monitors {
            // Beats arrive oldest-first; the newest one is the verdict. A
            // monitor with no beats yet is genuinely pending, not down.
            let last = hb.beats.get(&m.id.to_string()).and_then(|b| b.last());
            let (state, ping_ms) = match last {
                Some(b) => (
                    ServiceState::from_code(b.status),
                    b.ping.map(|p| p.max(0.0).round() as u32),
                ),
                None => (ServiceState::Pending, None),
            };
            out.push(Service {
                state,
                ping_ms,
                uptime24: hb.uptime.get(&format!("{}_24", m.id)).copied(),
                name: m.name,
            });
        }
    }
    Ok(out)
}

/// Fetch and join a status page. Blocking — call it from `spawn_blocking`,
/// the way every git subprocess is run.
pub fn fetch(base_url: &str, slug: &str) -> Result<Vec<Service>> {
    let base = base_url.trim_end_matches('/');
    let get = |url: String| -> Result<String> {
        ureq::get(&url)
            .timeout(std::time::Duration::from_secs(8))
            .call()
            .map_err(|e| match e {
                // The status body is a paragraph of HTML; the code is the fact.
                ureq::Error::Status(code, _) => anyhow!("HTTP {code}"),
                other => anyhow!("{other}"),
            })
            .with_context(|| format!("GET {url}"))?
            .into_string()
            .with_context(|| format!("read {url}"))
    };
    let page = get(format!("{base}/api/status-page/{slug}"))?;
    let heartbeat = get(format!("{base}/api/status-page/heartbeat/{slug}"))?;
    parse(&page, &heartbeat)
}

/// Which dashboard repo a monitor belongs to, if any.
///
/// The explicit `[uptime_kuma.map]` entry wins; without one, a monitor whose
/// name matches a repo's short name (or full `owner/name`) case-insensitively
/// maps itself — a monitor called `backend` finds `muufree/backend` with no
/// config at all. `None` is fine: the service still counts in the header
/// tally, it just decorates no row.
pub fn repo_for_service<'a>(
    name: &str,
    explicit: &'a HashMap<String, String>,
    repo_specs: &'a [String],
) -> Option<String> {
    if let Some(spec) = explicit.get(name) {
        return Some(spec.clone());
    }
    let needle = name.trim().to_lowercase();
    repo_specs
        .iter()
        .find(|spec| {
            let short = spec.rsplit('/').next().unwrap_or(spec);
            short.to_lowercase() == needle || spec.to_lowercase() == needle
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed captures of a real Kuma 1.x status page.
    const PAGE: &str = r#"{"config":{"slug":"all"},"incidents":[],
        "publicGroupList":[{"id":2,"name":"Services","weight":1,"monitorList":[
            {"id":2,"name":"muufree.com","sendUrl":0,"type":"keyword"},
            {"id":3,"name":"API","sendUrl":0,"type":"http"},
            {"id":9,"name":"brand-new","sendUrl":0,"type":"http"}]}],
        "maintenanceList":[]}"#;
    const BEATS: &str = r#"{"heartbeatList":{
            "2":[{"status":0,"time":"2026-08-18 06:00:00.000","msg":"timeout","ping":null},
                 {"status":1,"time":"2026-08-18 06:03:00.000","msg":"","ping":68}],
            "3":[{"status":1,"time":"2026-08-18 06:00:00.000","msg":"","ping":40},
                 {"status":0,"time":"2026-08-18 06:03:00.000","msg":"503","ping":null}]},
        "uptimeList":{"2_24":1,"3_24":0.97}}"#;

    #[test]
    #[ignore = "live probe: KUMA_URL=https://up.example.com KUMA_SLUG=default \
                cargo test kuma_live -- --ignored --nocapture"]
    fn kuma_live() {
        let url = std::env::var("KUMA_URL").expect("set KUMA_URL to a Kuma base URL");
        let slug = std::env::var("KUMA_SLUG").unwrap_or_else(|_| "default".into());
        let svcs = fetch(&url, &slug).unwrap();
        for s in &svcs {
            println!(
                "{:<24} {:?}  ping={:?}  24h={:?}",
                s.name, s.state, s.ping_ms, s.uptime24
            );
        }
        assert!(!svcs.is_empty(), "a published status page has monitors");
    }

    #[test]
    fn the_newest_beat_is_the_verdict() {
        let svcs = parse(PAGE, BEATS).unwrap();
        let by_name = |n: &str| svcs.iter().find(|s| s.name == n).unwrap();
        // Recovered: down an hour ago, up on the latest beat.
        let site = by_name("muufree.com");
        assert_eq!(site.state, ServiceState::Up);
        assert_eq!(site.ping_ms, Some(68));
        assert_eq!(site.uptime24, Some(1.0));
        // Broke: up first, down on the latest beat.
        let api = by_name("API");
        assert_eq!(api.state, ServiceState::Down);
        assert_eq!(api.uptime24, Some(0.97));
        // No beats yet is a monitor still warming up, not an outage.
        assert_eq!(by_name("brand-new").state, ServiceState::Pending);
    }

    #[test]
    fn monitors_find_their_repo_by_name_unless_told_otherwise() {
        let repos = vec!["muufree/backend".to_string(), "muufree/website".to_string()];
        let mut map = HashMap::new();
        assert_eq!(
            repo_for_service("Backend", &map, &repos).as_deref(),
            Some("muufree/backend"),
            "case-insensitive short-name match needs no config"
        );
        assert_eq!(repo_for_service("API", &map, &repos), None, "no guessing");
        // The explicit map covers what names alone cannot.
        map.insert("API".into(), "muufree/backend".into());
        assert_eq!(
            repo_for_service("API", &map, &repos).as_deref(),
            Some("muufree/backend")
        );
    }
}
