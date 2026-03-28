use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, Read, Write},
    path::PathBuf,
    thread,
    time::Duration,
};

// ---------------------------------------------------------------------------
// Window config schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    windows: Vec<UsageWindow>,
    task_size_thresholds: TaskThresholds,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct UsageWindow {
    id: String,
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    source: String,
    active_range: DateRange,
    tiers: Vec<Tier>,
    #[serde(default)]
    plans: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct DateRange {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct Tier {
    id: String,
    label: String,
    multiplier: f64,
    favorable: bool,
    schedule: Schedule,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Schedule {
    Recurring {
        days: Vec<String>,
        utc_start: String,
        utc_end: String,
    },
    InverseRecurring {
        base: Box<Schedule>,
    },
    Always,
}

#[derive(Debug, Deserialize)]
struct TaskThresholds {
    defer_at_multiplier_below: f64,
    sizes: HashMap<String, SizeConfig>,
}

#[derive(Debug, Deserialize)]
struct SizeConfig {
    defer: bool,
    #[allow(dead_code)]
    description: String,
}

// ---------------------------------------------------------------------------
// Claude Code statusline JSON schema (piped to stdin on each update)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct StatuslineInput {
    session_id: Option<String>,
    model: Option<StatuslineModel>,
    context_window: Option<ContextWindowInfo>,
    cost: Option<CostInfo>,
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Deserialize)]
struct StatuslineModel {
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct ContextWindowInfo {
    used_percentage: Option<f64>,
    current_usage: Option<CurrentUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct CurrentUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CostInfo {
    total_cost_usd: Option<f64>,
    total_duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct RateLimits {
    five_hour: Option<RateLimitWindow>,
    seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Deserialize)]
struct RateLimitWindow {
    used_percentage: Option<f64>,
    resets_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Usage tracking: persisted daily/weekly token state
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct UsageState {
    // session_id -> last seen cumulative total (for delta computation)
    sessions: HashMap<String, u64>,
    // "2026-03-15" -> token total
    daily: HashMap<String, u64>,
    // "2026-W11" -> token total
    weekly: HashMap<String, u64>,
}

fn usage_state_path() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("usage-tracker.json"))
}

fn load_usage_state() -> UsageState {
    usage_state_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_usage_state(state: &UsageState) {
    let Some(path) = usage_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(path, json);
    }
}

fn iso_week_key(now: DateTime<Utc>) -> String {
    let iw = now.iso_week();
    format!("{}-W{:02}", iw.year(), iw.week())
}

fn update_usage(state: &mut UsageState, session_id: &str, new_total: u64, now: DateTime<Utc>) {
    let last = state.sessions.get(session_id).copied().unwrap_or(0);
    let delta = if new_total >= last {
        new_total - last
    } else {
        new_total
    };

    if delta > 0 {
        let day = now.format("%Y-%m-%d").to_string();
        let week = iso_week_key(now);
        *state.daily.entry(day).or_insert(0) += delta;
        *state.weekly.entry(week).or_insert(0) += delta;
    }

    state.sessions.insert(session_id.to_string(), new_total);

    // Trim to 50 sessions (arbitrary eviction since HashMap has no order)
    if state.sessions.len() > 50 {
        let drain: Vec<_> = state
            .sessions
            .keys()
            .take(state.sessions.len() - 25)
            .cloned()
            .collect();
        for k in drain {
            state.sessions.remove(&k);
        }
    }

    // Prune old daily (>30 days) and weekly (>12 weeks) entries
    let cutoff_day = (now - chrono::Duration::days(30))
        .format("%Y-%m-%d")
        .to_string();
    state.daily.retain(|k, _| k.as_str() >= cutoff_day.as_str());
    let cutoff_week = now - chrono::Duration::weeks(12);
    let cutoff_week_key = iso_week_key(cutoff_week);
    state
        .weekly
        .retain(|k, _| k.as_str() >= cutoff_week_key.as_str());
}

fn claude_config_dir() -> Option<PathBuf> {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{}/.claude", h)))
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Anthropic API status (via Statuspage.io) + connectivity
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ComponentStatus {
    name: String,
    status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct IncidentInfo {
    name: String,
    impact: String,
    status: String,
    shortlink: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MaintenanceInfo {
    name: String,
    scheduled_for: String,
    scheduled_until: String,
    status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ApiStatusCache {
    fetched_at: String,
    indicator: String,
    description: String,
    #[serde(default)]
    next_retry_at: Option<String>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    components: Vec<ComponentStatus>,
    #[serde(default)]
    incidents: Vec<IncidentInfo>,
    #[serde(default)]
    scheduled_maintenances: Vec<MaintenanceInfo>,
}

#[derive(Debug, Clone)]
struct ApiStatus {
    indicator: String,
    description: String,
    age_secs: i64,
    online: bool,
    components: Vec<ComponentStatus>,
    incidents: Vec<IncidentInfo>,
    scheduled_maintenances: Vec<MaintenanceInfo>,
}

impl ApiStatus {
    fn has_incident(&self) -> bool {
        self.indicator != "none"
    }

    fn age_label(&self) -> String {
        let s = self.age_secs;
        match s {
            s if s < 60 => "just now".into(),
            s if s < 3600 => format!("{}m ago", s / 60),
            s if s < 86400 => format!("{}h ago", s / 3600),
            s => format!("{}d ago", s / 86400),
        }
    }
}

fn api_status_cache_path() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("api-status-cache.json"))
}

fn load_cached_api_status() -> Option<ApiStatusCache> {
    let path = api_status_cache_path()?;
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn cache_age_secs(cache: &ApiStatusCache) -> i64 {
    cache
        .fetched_at
        .parse::<DateTime<Utc>>()
        .map(|t| (Utc::now() - t).num_seconds())
        .unwrap_or(i64::MAX)
}

fn save_api_status_cache(cache: &ApiStatusCache) {
    let Some(path) = api_status_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cache) {
        let _ = fs::write(path, json);
    }
}

/// Backoff seconds for consecutive failures: 5, 10, 20, 40, 80, 160, 300 (cap).
fn backoff_secs(failures: u32) -> i64 {
    (5 * 2_i64.pow(failures.min(6))).min(300)
}

fn should_skip_fetch() -> bool {
    load_cached_api_status()
        .and_then(|c| c.next_retry_at)
        .and_then(|t| t.parse::<DateTime<Utc>>().ok())
        .is_some_and(|t| Utc::now() < t)
}

fn record_fetch_failure() {
    if let Some(mut cache) = load_cached_api_status() {
        cache.consecutive_failures += 1;
        let delay = backoff_secs(cache.consecutive_failures);
        cache.next_retry_at = Some((Utc::now() + chrono::Duration::seconds(delay)).to_rfc3339());
        save_api_status_cache(&cache);
    }
}

fn fetch_api_status_raw() -> Option<ApiStatusCache> {
    let body_str = ureq::get("https://status.claude.com/api/v2/summary.json")
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let body: serde_json::Value = serde_json::from_str(&body_str).ok()?;
    let status = body.get("status")?;

    let relevant_components = ["Claude API", "Claude Code"];
    let components: Vec<ComponentStatus> = body
        .get("components")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let name = c.get("name")?.as_str()?;
                    if relevant_components.contains(&name) {
                        Some(ComponentStatus {
                            name: name.to_string(),
                            status: c
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let incidents: Vec<IncidentInfo> = body
        .get("incidents")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|i| {
                    Some(IncidentInfo {
                        name: i.get("name")?.as_str()?.to_string(),
                        impact: i
                            .get("impact")
                            .and_then(|v| v.as_str())
                            .unwrap_or("none")
                            .to_string(),
                        status: i
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        shortlink: i
                            .get("shortlink")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let scheduled_maintenances: Vec<MaintenanceInfo> = body
        .get("scheduled_maintenances")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(MaintenanceInfo {
                        name: m.get("name")?.as_str()?.to_string(),
                        scheduled_for: m
                            .get("scheduled_for")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        scheduled_until: m
                            .get("scheduled_until")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        status: m
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let cache = ApiStatusCache {
        fetched_at: Utc::now().to_rfc3339(),
        indicator: status
            .get("indicator")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        description: status
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string(),
        next_retry_at: None,
        consecutive_failures: 0,
        components,
        incidents,
        scheduled_maintenances,
    };
    save_api_status_cache(&cache);
    Some(cache)
}

fn to_api_status(cache: &ApiStatusCache, online: bool) -> ApiStatus {
    ApiStatus {
        indicator: cache.indicator.clone(),
        description: cache.description.clone(),
        age_secs: cache_age_secs(cache),
        online,
        components: cache.components.clone(),
        incidents: cache.incidents.clone(),
        scheduled_maintenances: cache.scheduled_maintenances.clone(),
    }
}

/// Try to fetch, respecting backoff. Returns true=online if succeeded.
fn try_fetch() -> Option<ApiStatusCache> {
    if should_skip_fetch() {
        return None;
    }
    match fetch_api_status_raw() {
        Some(cache) => Some(cache),
        None => {
            record_fetch_failure();
            None
        }
    }
}

/// Refresh if stale (5 min TTL), fall back to stale cache on fetch failure.
/// Respects exponential backoff on repeated failures.
fn get_api_status_fresh() -> Option<ApiStatus> {
    if let Some(cached) = load_cached_api_status() {
        if cache_age_secs(&cached) <= 300 {
            return Some(to_api_status(&cached, true));
        }
    }
    if let Some(fresh) = try_fetch() {
        return Some(to_api_status(&fresh, true));
    }
    // Fetch failed or skipped — use stale cache, mark offline
    load_cached_api_status().map(|c| to_api_status(&c, false))
}

/// Always fetch, ignoring backoff (for dedicated api-status subcommand).
fn get_api_status_forced() -> Option<ApiStatus> {
    if let Some(fresh) = fetch_api_status_raw() {
        return Some(to_api_status(&fresh, true));
    }
    record_fetch_failure();
    load_cached_api_status().map(|c| to_api_status(&c, false))
}

// ---------------------------------------------------------------------------
// Direct API connectivity probe (detects 5xx errors not on status page)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct ApiConnectivityCache {
    checked_at: String,
    http_status: u16,
}

impl ApiConnectivityCache {
    fn is_server_error(&self) -> bool {
        self.http_status >= 500
    }

    /// Labels matching Anthropic's documented error types.
    fn error_label(&self) -> &'static str {
        match self.http_status {
            529 => "overloaded",     // overloaded_error
            500 => "internal error", // api_error
            502 => "bad gateway",
            503 => "unavailable",
            _ if self.http_status >= 500 => "server error",
            _ => "ok",
        }
    }

    /// Formatted display: "overloaded (529)"
    fn display(&self) -> String {
        format!("{} ({})", self.error_label(), self.http_status)
    }
}

fn api_connectivity_cache_path() -> Option<PathBuf> {
    claude_config_dir().map(|d| d.join("api-connectivity-cache.json"))
}

fn load_api_connectivity_cache() -> Option<ApiConnectivityCache> {
    let path = api_connectivity_cache_path()?;
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_api_connectivity_cache(cache: &ApiConnectivityCache) {
    let Some(path) = api_connectivity_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cache) {
        let _ = fs::write(path, json);
    }
}

fn connectivity_cache_age_secs(cache: &ApiConnectivityCache) -> i64 {
    cache
        .checked_at
        .parse::<DateTime<Utc>>()
        .map(|t| (Utc::now() - t).num_seconds())
        .unwrap_or(i64::MAX)
}

/// Probe the API endpoint directly to detect 5xx server errors.
/// Uses a short timeout to avoid blocking the statusline.
fn probe_api_connectivity() -> Option<ApiConnectivityCache> {
    let resp = ureq::get("https://api.anthropic.com/v1/messages")
        .timeout(Duration::from_millis(1500))
        .call();
    let http_status = match &resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => *code,
        Err(_) => return None, // network error, can't determine
    };
    let cache = ApiConnectivityCache {
        checked_at: Utc::now().to_rfc3339(),
        http_status,
    };
    save_api_connectivity_cache(&cache);
    Some(cache)
}

/// Read from cache only (for latency-sensitive paths like statusline).
fn get_api_connectivity_cached() -> Option<ApiConnectivityCache> {
    load_api_connectivity_cache()
}

/// Refresh if stale (60s TTL), fall back to stale cache on probe failure.
fn get_api_connectivity_fresh() -> Option<ApiConnectivityCache> {
    let cached = load_api_connectivity_cache();
    if let Some(ref c) = cached {
        if connectivity_cache_age_secs(c) <= 60 {
            return cached;
        }
    }
    probe_api_connectivity().or(cached)
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

fn config_paths() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(dir) = claude_config_dir() {
        v.push(dir.join("usage-windows.json"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        v.push(cwd.join(".claude").join("usage-windows.json"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("usage-windows.json"));
        }
    }
    v
}

fn load_config() -> Result<Config> {
    for path in config_paths() {
        if let Ok(data) = fs::read_to_string(&path) {
            return serde_json::from_str(&data)
                .with_context(|| format!("invalid config at {}", path.display()));
        }
    }
    serde_json::from_str(DEFAULT_WINDOWS_JSON).context("embedded default windows.json invalid")
}

// ---------------------------------------------------------------------------
// Schedule evaluation
// ---------------------------------------------------------------------------

fn parse_weekday(s: &str) -> Option<Weekday> {
    match s.to_lowercase().as_str() {
        "sun" => Some(Weekday::Sun),
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        _ => None,
    }
}

fn parse_utc_time(hhmm: &str, ref_dt: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let t =
        NaiveTime::parse_from_str(hhmm, "%H:%M").with_context(|| format!("bad time: {hhmm}"))?;
    Ok(ref_dt.date_naive().and_time(t).and_utc())
}

fn matches_recurring(days: &[String], start: &str, end: &str, t: DateTime<Utc>) -> bool {
    let wd = t.weekday();
    if !days
        .iter()
        .filter_map(|d| parse_weekday(d))
        .any(|d| d == wd)
    {
        return false;
    }
    let Ok(s) = parse_utc_time(start, t) else {
        return false;
    };
    let Ok(e) = parse_utc_time(end, t) else {
        return false;
    };
    t >= s && t < e
}

fn matches_schedule(sched: &Schedule, t: DateTime<Utc>) -> bool {
    match sched {
        Schedule::Recurring {
            days,
            utc_start,
            utc_end,
        } => matches_recurring(days, utc_start, utc_end, t),
        Schedule::InverseRecurring { base } => !matches_schedule(base, t),
        Schedule::Always => true,
    }
}

fn mins_until_boundary(sched: &Schedule, now: DateTime<Utc>, want_start: bool) -> Option<u32> {
    for i in 1..=(7 * 24 * 60u32) {
        let c = now + chrono::Duration::minutes(i as i64);
        if want_start == matches_schedule(sched, c) {
            return Some(i);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Status types and evaluation
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WindowStatus {
    active: bool,
    reason: Option<String>,
    starts_in: Option<u32>,
    window: UsageWindow,
    tier: Option<Tier>,
    multiplier: f64,
    favorable: bool,
    mins_until_change: Option<u32>,
    mins_until_favorable: Option<u32>,
    promo_ends_in: u32,
}

#[derive(Debug)]
struct Status {
    now: DateTime<Utc>,
    multiplier: f64,
    favorable: bool,
    active_windows: Vec<WindowStatus>,
    inactive_windows: Vec<WindowStatus>,
    mins_until_favorable: Option<u32>,
    mins_until_change: Option<u32>,
    thresholds: TaskThresholds,
}

fn evaluate_window(w: UsageWindow, now: DateTime<Utc>) -> WindowStatus {
    if now < w.active_range.start {
        let starts_in = (w.active_range.start - now).num_minutes().max(0) as u32;
        return WindowStatus {
            active: false,
            reason: Some("not_started".into()),
            starts_in: Some(starts_in),
            window: w,
            tier: None,
            multiplier: 1.0,
            favorable: false,
            mins_until_change: None,
            mins_until_favorable: None,
            promo_ends_in: 0,
        };
    }
    if now > w.active_range.end {
        return WindowStatus {
            active: false,
            reason: Some("ended".into()),
            starts_in: None,
            window: w,
            tier: None,
            multiplier: 1.0,
            favorable: false,
            mins_until_change: None,
            mins_until_favorable: None,
            promo_ends_in: 0,
        };
    }

    let active_tier = w
        .tiers
        .iter()
        .find(|t| matches_schedule(&t.schedule, now))
        .cloned();
    let multiplier = active_tier.as_ref().map(|t| t.multiplier).unwrap_or(1.0);
    let favorable = active_tier.as_ref().map(|t| t.favorable).unwrap_or(false);
    let mins_until_change = active_tier
        .as_ref()
        .and_then(|t| mins_until_boundary(&t.schedule, now, false));
    let mins_until_favorable = if !favorable {
        w.tiers
            .iter()
            .find(|t| t.favorable)
            .and_then(|t| mins_until_boundary(&t.schedule, now, true))
    } else {
        None
    };
    let promo_ends_in = (w.active_range.end - now).num_minutes().max(0) as u32;

    WindowStatus {
        active: true,
        reason: None,
        starts_in: None,
        window: w,
        tier: active_tier,
        multiplier,
        favorable,
        mins_until_change,
        mins_until_favorable,
        promo_ends_in,
    }
}

fn evaluate(config: Config, now: DateTime<Utc>) -> Status {
    let mut active = vec![];
    let mut inactive = vec![];
    let mut best_mult = 1.0f64;
    let mut favorable = false;

    for w in config.windows {
        let ws = evaluate_window(w, now);
        if ws.active {
            if ws.multiplier > best_mult {
                best_mult = ws.multiplier;
            }
            if ws.favorable {
                favorable = true;
            }
            active.push(ws);
        } else {
            inactive.push(ws);
        }
    }

    Status {
        now,
        multiplier: best_mult,
        favorable,
        mins_until_favorable: active
            .iter()
            .filter(|w| !w.favorable)
            .filter_map(|w| w.mins_until_favorable)
            .min(),
        mins_until_change: active.iter().filter_map(|w| w.mins_until_change).min(),
        active_windows: active,
        inactive_windows: inactive,
        thresholds: config.task_size_thresholds,
    }
}

fn should_defer(size: &str, s: &Status) -> bool {
    s.multiplier < s.thresholds.defer_at_multiplier_below
        && s.thresholds
            .sizes
            .get(size)
            .map(|c| c.defer)
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn fmt_mins(m: u32) -> String {
    match m {
        m if m >= 525_600 => "ongoing".into(),
        m if m >= 1440 => format!("{}d {}h", m / 1440, (m % 1440) / 60),
        m if m >= 60 => format!("{}h {:02}m", m / 60, m % 60),
        m => format!("{}m", m),
    }
}

fn fmt_mins_opt(m: Option<u32>) -> String {
    m.map(fmt_mins).unwrap_or_else(|| "—".into())
}

fn fmt_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}m", n as f64 / 1_000_000.0),
        n if n >= 10_000 => format!("{:.0}k", n as f64 / 1_000.0),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

fn ctx_bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled.min(width)),
        "░".repeat(width - filled.min(width))
    )
}

fn paced_bar(pct: f64, pace_pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let pace_pos = ((pace_pct / 100.0) * width as f64).round() as usize;
    let mut chars: Vec<&str> = Vec::with_capacity(width);
    for i in 0..width {
        if i == pace_pos && pace_pos < width {
            if i < filled {
                chars.push("▊");
            } else {
                chars.push("┊");
            }
        } else if i < filled {
            chars.push("█");
        } else {
            chars.push("░");
        }
    }
    chars.concat()
}

fn ansi(codes: &str, text: &str) -> String {
    format!("\x1b[{}m{}\x1b[0m", codes, text)
}

fn ctx_colored(pct: f64, text: &str) -> String {
    if pct < 50.0 {
        ansi("32", text)
    } else if pct < 80.0 {
        ansi("33", text)
    } else {
        ansi("31;1", text)
    }
}

fn send_notification(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            body.replace('\"', "\\\""),
            title.replace('\"', "\\\""),
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .output();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, body])
            .output();
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "claude-usage",
    about = "Claude usage window optimizer: status bar, token tracking, defer logic",
    version,
    long_version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("TARGET"), ")"),
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Human-readable status (default)
    Status,
    /// Claude Code status bar (reads JSON from stdin, outputs formatted bar)
    Statusline,
    /// Compact label for PS1/Starship (e.g. "⚡2x")
    Label,
    /// tmux status bar segment with ANSI color codes
    Tmux,
    /// Machine-readable JSON
    Json,
    /// List all configured windows
    Windows,
    /// Show peak/off-peak schedule across timezones
    Schedule,
    /// Block until a favorable window opens
    Wait,
    /// Watch for status changes with desktop notifications
    Watch,
    /// Check if a task should be deferred (small|medium|large|xl)
    Defer {
        #[arg(default_value = "large")]
        size: String,
    },
    /// Check Anthropic API status (via status.anthropic.com)
    ApiStatus,
    /// Write windows.json to ~/.claude/ and register statusLine in settings.json
    Init {
        /// Overwrite existing usage-windows.json with the embedded default
        #[arg(long)]
        force: bool,
    },
    /// MCP server over stdio (JSON-RPC 2.0)
    Mcp,
}

fn main() -> Result<()> {
    let cmd = Cli::parse().command.unwrap_or(Cmd::Status);

    if let Cmd::Init { force } = cmd {
        return run_init(force);
    }
    if let Cmd::Mcp = cmd {
        return run_mcp();
    }
    if let Cmd::ApiStatus = cmd {
        run_api_status();
        return Ok(());
    }
    if let Cmd::Watch = cmd {
        return run_watch();
    }

    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            // Stay silent for display-only commands so we don't pollute PS1 / status bar
            if matches!(cmd, Cmd::Label | Cmd::Tmux | Cmd::Statusline) {
                return Ok(());
            }
            return Err(e);
        }
    };

    let now = Utc::now();
    let s = evaluate(config, now);

    match cmd {
        Cmd::Status => run_status(&s),
        Cmd::Statusline => run_statusline(&s),
        Cmd::Label => run_label(&s),
        Cmd::Tmux => run_tmux(&s),
        Cmd::Json => run_json(&s)?,
        Cmd::Windows => run_windows(&s),
        Cmd::Schedule => run_schedule(&s),
        Cmd::Wait => run_wait(&s),
        Cmd::Defer { size } => run_defer(&s, &size),
        Cmd::Init { .. } | Cmd::Mcp | Cmd::ApiStatus | Cmd::Watch => unreachable!(),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run_statusline: Claude Code statusLine integration
//
// Claude Code pipes JSON on every turn update. We:
//   1. Parse the JSON for model, ctx %, cumulative tokens, cost
//   2. Compute delta from last-seen session total → update daily/weekly state file
//   3. Emit two lines:
//        Line 1 (promo only): ⚡2x OFF-PEAK  ends in 13h 57m
//        Line 2:              sonnet-4-5 │ ████░░░░ 23% │ sess 12.3k │ day 45.6k │ wk 234k │ $0.024
// ---------------------------------------------------------------------------

fn run_statusline(s: &Status) {
    let now = s.now;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let cc: StatuslineInput = serde_json::from_str(&raw).unwrap_or_default();

    let session_total = cc
        .context_window
        .as_ref()
        .and_then(|cw| cw.current_usage.as_ref())
        .map(|u| {
            u.input_tokens.unwrap_or(0)
                + u.output_tokens.unwrap_or(0)
                + u.cache_creation_input_tokens.unwrap_or(0)
                + u.cache_read_input_tokens.unwrap_or(0)
        })
        .unwrap_or(0);

    // Update persistent state and retrieve today/week totals
    let mut state = load_usage_state();
    if session_total > 0 {
        let sid = cc.session_id.as_deref().unwrap_or("default");
        update_usage(&mut state, sid, session_total, now);
        save_usage_state(&state);
    }
    let day_key = now.format("%Y-%m-%d").to_string();
    let week_key = iso_week_key(now);
    let day_total = state.daily.get(&day_key).copied().unwrap_or(0);
    let week_total = state.weekly.get(&week_key).copied().unwrap_or(0);

    // ── Line 1: promo/peak status + connectivity/incident ─────────────────
    let api = get_api_status_fresh();
    let conn = get_api_connectivity_cached();
    let suffix = if let Some(c) = conn.as_ref().filter(|c| c.is_server_error()) {
        ansi("31;1", &format!("  ⚠️ API {}", c.display()))
    } else {
        match &api {
            Some(s) if s.has_incident() => {
                let (icon, color) = match s.indicator.as_str() {
                    "critical" => ("⛔", "31;1"),
                    "major" => ("⚠️", "33;1"),
                    _ => ("⚠", "33"),
                };
                let label = s
                    .incidents
                    .first()
                    .map(|i| i.name.as_str())
                    .unwrap_or(s.description.as_str());
                ansi(color, &format!("  {} {}", icon, label))
            }
            Some(s) if !s.online => ansi("90", "  📡?"),
            _ => String::new(),
        }
    };

    if !s.active_windows.is_empty() {
        if s.favorable {
            println!(
                "{} {}  ends in {}{}",
                ansi("32;1", &format!("⚡{:.0}x", s.multiplier)),
                ansi("32;1", "OFF-PEAK"),
                fmt_mins_opt(s.mins_until_change),
                suffix
            );
        } else {
            println!(
                "{} {}  {:.0}x in {}{}",
                ansi("33;1", &format!("·{:.0}x", s.multiplier)),
                ansi("33;1", "PEAK"),
                s.active_windows
                    .iter()
                    .filter(|w| w.favorable)
                    .map(|w| w.multiplier)
                    .next()
                    .unwrap_or(2.0),
                fmt_mins_opt(s.mins_until_favorable),
                suffix
            );
        }
    } else if !suffix.is_empty() {
        println!("{}", suffix);
    }

    // ── Line 2: model │ ctx bar │ token counts │ cost ────────────────────────
    let mut parts: Vec<String> = vec![];

    if let Some(ref m) = cc.model {
        // "claude-sonnet-4-5-20251022" → "sonnet-4-5"
        let name = m
            .display_name
            .strip_prefix("claude-")
            .unwrap_or(&m.display_name)
            .splitn(4, '-')
            .take(3)
            .collect::<Vec<_>>()
            .join("-");
        parts.push(ansi("36;1", &name));
    }

    if let Some(pct) = cc.context_window.as_ref().and_then(|c| c.used_percentage) {
        if pct > 10.0 {
            let bar = ctx_bar(pct, 8);
            let text = format!("ctx {} {:.0}%", bar, pct);
            parts.push(ctx_colored(pct, &text));
        } else if pct > 0.0 {
            parts.push(ctx_colored(pct, &format!("ctx {:.0}%", pct)));
        }
    }

    if day_total > 0 {
        parts.push(format!("d {}", fmt_tokens(day_total)));
    }
    if week_total > 0 {
        parts.push(format!("w {}", fmt_tokens(week_total)));
    }

    if let Some(ref cost_info) = cc.cost {
        if let Some(cost) = cost_info.total_cost_usd {
            if cost > 0.001 {
                let burn = cost_info
                    .total_duration_ms
                    .filter(|&ms| ms > 60_000)
                    .map(|ms| cost / (ms as f64 / 3_600_000.0));
                if let Some(rate) = burn {
                    parts.push(ansi("90", &format!("~${:.2} (${:.2}/h)", cost, rate)));
                } else {
                    parts.push(ansi("90", &format!("~${:.2}", cost)));
                }
            }
        }
    }

    if let Some(ref rl) = cc.rate_limits {
        let now_ts = now.timestamp();
        let window_secs: &[(&str, &Option<RateLimitWindow>, i64)] =
            &[("5h", &rl.five_hour, 18000), ("7d", &rl.seven_day, 604800)];
        for &(label, window, duration) in window_secs {
            if let Some(w) = window {
                if let Some(pct) = w.used_percentage {
                    if pct > 50.0 {
                        let pace_pct = w.resets_at.filter(|&ts| ts > now_ts).map(|ts| {
                            let elapsed = duration - (ts - now_ts);
                            (elapsed as f64 / duration as f64 * 100.0).clamp(0.0, 100.0)
                        });
                        let bar = if let Some(pp) = pace_pct {
                            paced_bar(pct, pp, 8)
                        } else {
                            ctx_bar(pct, 8)
                        };
                        let reset_suffix = if pct > 80.0 {
                            w.resets_at
                                .filter(|&ts| ts > now_ts)
                                .map(|ts| {
                                    let mins = (ts - now_ts) / 60;
                                    if mins >= 60 {
                                        format!(" ↻{}h{}m", mins / 60, mins % 60)
                                    } else {
                                        format!(" ↻{}m", mins)
                                    }
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let text = format!("{} {} {:.0}%{}", label, bar, pct, reset_suffix);
                        parts.push(ctx_colored(pct, &text));
                    } else {
                        parts.push(ctx_colored(pct, &format!("{} {:.0}%", label, pct)));
                    }
                }
            }
        }
    }

    if !parts.is_empty() {
        println!("{}", parts.join(" │ "));
    }
}

// ---------------------------------------------------------------------------
// Other commands
// ---------------------------------------------------------------------------

fn fmt_api_status_line(status: &ApiStatus) -> String {
    let age = status.age_label();
    let conn = if status.online { "" } else { " (offline)" };
    let mut lines = Vec::new();

    if status.has_incident() {
        let icon = match status.indicator.as_str() {
            "critical" => "🔴",
            "major" => "🟠",
            _ => "🟡",
        };
        lines.push(format!(
            "{} API: {} ({}){}",
            icon, status.description, age, conn
        ));
    } else {
        lines.push(format!("🟢 API: {} ({}){}", status.description, age, conn));
    }

    for inc in &status.incidents {
        lines.push(format!("  ⚡ {} [{}]", inc.name, inc.impact));
    }

    for comp in &status.components {
        if comp.status != "operational" {
            let display = comp.status.replace('_', " ");
            lines.push(format!("  ↳ {}: {}", comp.name, display));
        }
    }

    for maint in &status.scheduled_maintenances {
        lines.push(format!(
            "  🔧 Maintenance: {} ({})",
            maint.name, maint.scheduled_for
        ));
    }

    lines.join("\n")
}

fn run_api_status() {
    let status = get_api_status_forced();
    let conn = probe_api_connectivity();

    match (&status, &conn) {
        (_, Some(c)) if c.is_server_error() => println!("🟠 API {}", c.display()),
        (Some(s), _) => println!("{}", fmt_api_status_line(s)),
        (None, Some(_)) => println!("🟢 API endpoint responding (status page unavailable)"),
        (None, None) => println!("⚪ API: Unable to reach status page or API endpoint"),
    }
}

fn run_status(s: &Status) {
    let status = get_api_status_fresh();
    let conn = get_api_connectivity_fresh();
    match (&status, &conn) {
        (_, Some(c)) if c.is_server_error() => println!("🟠 API {}", c.display()),
        (Some(s), _) => println!("{}", fmt_api_status_line(s)),
        _ => {}
    }

    if s.active_windows.is_empty() {
        println!("No active Claude usage promotions. Standard rates apply.");
        for w in &s.inactive_windows {
            if w.reason.as_deref() == Some("not_started") {
                println!(
                    "Next: {}, starts in {}",
                    w.window.label,
                    fmt_mins_opt(w.starts_in)
                );
            }
        }
        return;
    }
    for w in &s.active_windows {
        let t = w
            .tier
            .as_ref()
            .map(|t| t.label.as_str())
            .unwrap_or("unknown");
        if w.favorable {
            println!(
                "🟢 {} ({:.0}x usage)\n   Ends in:      {}",
                t,
                w.multiplier,
                fmt_mins_opt(w.mins_until_change)
            );
        } else {
            println!(
                "🔴 {} ({:.0}x usage)\n   Favorable in: {}",
                t,
                w.multiplier,
                fmt_mins_opt(w.mins_until_favorable)
            );
        }
        let duration = fmt_mins(w.promo_ends_in);
        if duration == "ongoing" {
            println!("   Promo: {} (ongoing)", w.window.label);
        } else {
            println!("   Promo: {} (ends in {})", w.window.label, duration);
        }
    }
}

fn run_label(s: &Status) {
    if s.active_windows.is_empty() {
        return;
    }
    print!(
        "{}{:.0}x",
        if s.favorable { "⚡" } else { "·" },
        s.multiplier
    );
}

fn run_tmux(s: &Status) {
    if s.active_windows.is_empty() {
        return;
    }
    if s.favorable {
        print!(
            "#[fg=colour46,bold]⚡{:.0}x#[fg=colour244] Claude#[default]",
            s.multiplier
        );
    } else {
        print!(
            "#[fg=colour208,bold]·{:.0}x#[fg=colour244] ({})#[default]",
            s.multiplier,
            fmt_mins_opt(s.mins_until_favorable)
        );
    }
}

fn run_json(s: &Status) -> Result<()> {
    #[derive(Serialize)]
    struct Out {
        now: String,
        multiplier: f64,
        favorable: bool,
        mins_until_favorable: Option<u32>,
        mins_until_change: Option<u32>,
        api_status: Option<ApiStatusOut>,
        active_windows: Vec<WinOut>,
        inactive_windows: Vec<WinOut>,
    }
    #[derive(Serialize)]
    struct ApiStatusOut {
        indicator: String,
        description: String,
        online: bool,
        age_secs: i64,
        api_server_error: bool,
        api_http_status: Option<u16>,
    }
    #[derive(Serialize)]
    struct WinOut {
        id: String,
        label: String,
        multiplier: f64,
        favorable: bool,
        tier_label: Option<String>,
        mins_until_change: Option<u32>,
        mins_until_favorable: Option<u32>,
        promo_ends_in_mins: u32,
        source: String,
    }
    let to_w = |w: &WindowStatus| WinOut {
        id: w.window.id.clone(),
        label: w.window.label.clone(),
        multiplier: w.multiplier,
        favorable: w.favorable,
        tier_label: w.tier.as_ref().map(|t| t.label.clone()),
        mins_until_change: w.mins_until_change,
        mins_until_favorable: w.mins_until_favorable,
        promo_ends_in_mins: w.promo_ends_in,
        source: w.window.source.clone(),
    };
    let conn = get_api_connectivity_fresh();
    let is_5xx = conn.as_ref().is_some_and(|c| c.is_server_error());
    let api_status = get_api_status_fresh().map(|s| ApiStatusOut {
        indicator: s.indicator,
        description: s.description,
        online: s.online,
        age_secs: s.age_secs,
        api_server_error: is_5xx,
        api_http_status: conn
            .as_ref()
            .filter(|c| c.is_server_error())
            .map(|c| c.http_status),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&Out {
            now: s.now.to_rfc3339(),
            multiplier: s.multiplier,
            favorable: s.favorable,
            mins_until_favorable: s.mins_until_favorable,
            mins_until_change: s.mins_until_change,
            api_status,
            active_windows: s.active_windows.iter().map(to_w).collect(),
            inactive_windows: s.inactive_windows.iter().map(to_w).collect(),
        })?
    );
    Ok(())
}

fn run_windows(s: &Status) {
    if s.active_windows.is_empty() && s.inactive_windows.is_empty() {
        println!("No windows configured. Run: claude-usage init");
        return;
    }
    for w in &s.active_windows {
        let tier = w
            .tier
            .as_ref()
            .map(|t| format!("{} ({:.0}x)", t.label, t.multiplier))
            .unwrap_or_else(|| "—".into());
        println!("[ACTIVE]   {}\n           Tier: {}", w.window.label, tier);
        if !w.window.source.is_empty() {
            println!("           Ref:  {}", w.window.source);
        }
        println!();
    }
    for w in &s.inactive_windows {
        let state = match w.reason.as_deref() {
            Some("not_started") => format!("STARTS IN {}", fmt_mins_opt(w.starts_in)),
            Some(r) => r.to_uppercase(),
            None => "UNKNOWN".into(),
        };
        println!("[{:<22}]  {}", state, w.window.label);
    }
}

fn get_local_tz() -> Option<Tz> {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|s| s.parse::<Tz>().ok())
}

const SCHEDULE_ZONES: &[(&str, &str)] = &[
    ("San Francisco", "America/Los_Angeles"),
    ("New York", "America/New_York"),
    ("London", "Europe/London"),
    ("Paris", "Europe/Paris"),
    ("Istanbul", "Europe/Istanbul"),
    ("Dubai", "Asia/Dubai"),
    ("Mumbai", "Asia/Kolkata"),
    ("Tokyo", "Asia/Tokyo"),
    ("Sydney", "Australia/Sydney"),
];

fn print_schedule_table(
    utc_start: &str,
    utc_end: &str,
    days: &[String],
    now: DateTime<Utc>,
    local_tz: Option<Tz>,
) {
    let day_list = days.join(", ");
    println!("  Peak: {} UTC {} – {}", day_list, utc_start, utc_end);
    println!();

    let today = now.date_naive();
    let start_naive = chrono::NaiveDateTime::new(
        today,
        NaiveTime::parse_from_str(utc_start, "%H:%M").unwrap_or_default(),
    );
    let end_naive = chrono::NaiveDateTime::new(
        today,
        NaiveTime::parse_from_str(utc_end, "%H:%M").unwrap_or_default(),
    );
    let start_utc = start_naive.and_utc();
    let end_utc = end_naive.and_utc();

    // Build zone list: static zones + local zone if not already represented
    let mut zones: Vec<(String, Tz)> = SCHEDULE_ZONES
        .iter()
        .filter_map(|(city, tz_name)| tz_name.parse::<Tz>().ok().map(|tz| (city.to_string(), tz)))
        .collect();

    if let Some(ltz) = local_tz {
        if !zones.iter().any(|(_, tz)| *tz == ltz) {
            zones.push((
                ltz.name()
                    .rsplit('/')
                    .next()
                    .unwrap_or("Local")
                    .replace('_', " "),
                ltz,
            ));
        }
    }

    println!(
        "    {:<16} {:>12}  {:>12}",
        "City", "Peak start", "Peak end"
    );
    println!(
        "    {:<16} {:>12}  {:>12}",
        "────────────────", "──────────", "──────────"
    );

    for (city, tz) in &zones {
        let local_start = start_utc.with_timezone(tz);
        let local_end = end_utc.with_timezone(tz);
        let marker = if local_tz.as_ref() == Some(tz) {
            "→ "
        } else {
            "  "
        };
        println!(
            "  {}{:<16} {:>12}  {:>12}",
            marker,
            city,
            local_start.format("%l:%M %p").to_string().trim(),
            local_end.format("%l:%M %p").to_string().trim(),
        );
    }
}

fn extract_recurring(schedule: &Schedule) -> Option<(&[String], &str, &str)> {
    match schedule {
        Schedule::Recurring {
            days,
            utc_start,
            utc_end,
        } => Some((days, utc_start, utc_end)),
        Schedule::InverseRecurring { base } => match base.as_ref() {
            Schedule::Recurring {
                days,
                utc_start,
                utc_end,
            } => Some((days, utc_start, utc_end)),
            _ => None,
        },
        Schedule::Always => None,
    }
}

fn run_schedule(s: &Status) {
    if s.active_windows.is_empty() {
        println!("No active windows. Run: claude-usage init");
        return;
    }

    let local_tz = get_local_tz();

    for w in &s.active_windows {
        println!("{}", w.window.label);
        let mut found_schedule = false;
        for tier in &w.window.tiers {
            if found_schedule {
                continue;
            }
            if let Some((days, utc_start, utc_end)) = extract_recurring(&tier.schedule) {
                found_schedule = true;
                print_schedule_table(utc_start, utc_end, days, s.now, local_tz);
            }
        }
        if !found_schedule {
            println!("  Schedule: always active");
        }
        println!();
    }
}

fn run_wait(s: &Status) {
    if s.favorable {
        eprintln!("✅ Already in favorable window.");
        return;
    }
    match s.mins_until_favorable {
        None => eprintln!("ℹ️  No favorable window scheduled."),
        Some(m) => {
            eprintln!("⏳ Waiting {} for favorable window...", fmt_mins(m));
            thread::sleep(Duration::from_secs(m as u64 * 60 + 30));
            eprintln!("✅ Favorable window active.");
        }
    }
}

fn run_watch() -> Result<()> {
    let mut last_favorable: Option<bool> = None;
    let mut last_incident: Option<bool> = None;

    loop {
        let config = load_config();
        let now = Utc::now();

        match config {
            Ok(config) => {
                let s = evaluate(config, now);

                let is_favorable = s.favorable && !s.active_windows.is_empty();
                if let Some(was_favorable) = last_favorable {
                    if is_favorable && !was_favorable {
                        let msg = format!("⚡ {:.0}x multiplier now active", s.multiplier);
                        println!("{} {}", now.format("%H:%M"), msg);
                        send_notification("Claude Usage", &msg);
                    } else if !is_favorable && was_favorable {
                        println!("{} · Favorable window ended", now.format("%H:%M"));
                    }
                } else if s.active_windows.is_empty() {
                    println!("{} No active promotions", now.format("%H:%M"));
                } else if is_favorable {
                    println!(
                        "{} ⚡ {:.0}x OFF-PEAK  ends in {}",
                        now.format("%H:%M"),
                        s.multiplier,
                        fmt_mins_opt(s.mins_until_change)
                    );
                } else {
                    println!(
                        "{} · {:.0}x PEAK  favorable in {}",
                        now.format("%H:%M"),
                        s.multiplier,
                        fmt_mins_opt(s.mins_until_favorable)
                    );
                }
                last_favorable = Some(is_favorable);

                let api = get_api_status_fresh();
                let has_incident = api.as_ref().is_some_and(|a| a.has_incident());
                if let Some(had_incident) = last_incident {
                    if has_incident && !had_incident {
                        let desc = api
                            .as_ref()
                            .map(|a| a.description.as_str())
                            .unwrap_or("Unknown");
                        let msg = format!("⚠️ {}", desc);
                        println!("{} {}", now.format("%H:%M"), msg);
                        send_notification("Claude API", &msg);
                    } else if !has_incident && had_incident {
                        println!("{} ✅ API incident resolved", now.format("%H:%M"));
                        send_notification(
                            "Claude API",
                            "Incident resolved — all systems operational",
                        );
                    }
                }
                last_incident = Some(has_incident);
            }
            Err(_) => {
                if last_favorable.is_none() {
                    println!("{} Watching... (no config found)", now.format("%H:%M"));
                    last_favorable = Some(false);
                }
            }
        }

        thread::sleep(Duration::from_secs(60));
    }
}

fn run_defer(s: &Status, size: &str) {
    if should_defer(size, s) {
        println!("⏸️  DEFER RECOMMENDED: {} at {:.0}x", size, s.multiplier);
        if let Some(m) = s.mins_until_favorable {
            println!("   Favorable window in: {}", fmt_mins(m));
        }
    } else {
        let reason = if s.multiplier >= s.thresholds.defer_at_multiplier_below {
            "already in favorable window"
        } else {
            "size not worth deferring"
        };
        println!("✅ PROCEED: {} at {:.0}x ({})", size, s.multiplier, reason);
    }
}

// ---------------------------------------------------------------------------
// MCP server (JSON-RPC 2.0 over stdio)
// ---------------------------------------------------------------------------

fn jsonrpc_response(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: &serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn run_mcp() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let resp = jsonrpc_error(&serde_json::Value::Null, -32700, "parse error");
                serde_json::to_writer(&mut stdout, &resp)?;
                writeln!(stdout)?;
                stdout.flush()?;
                continue;
            }
        };

        // Notifications (no id) are silently ignored
        let Some(id) = msg.get("id") else {
            continue;
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let resp = match method {
            "initialize" => jsonrpc_response(
                id,
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "claude-usage",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            ),

            "tools/list" => jsonrpc_response(
                id,
                serde_json::json!({
                    "tools": [{
                        "name": "should_defer_task",
                        "description": "Check whether a task should be deferred based on the current Claude usage window. Returns the current multiplier and a defer recommendation.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "size": {
                                    "type": "string",
                                    "description": "Task size: small, medium, large, or xl",
                                    "default": "large",
                                }
                            }
                        }
                    }]
                }),
            ),

            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_default();
                let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");

                if tool_name != "should_defer_task" {
                    jsonrpc_error(id, -32602, &format!("unknown tool: {}", tool_name))
                } else {
                    let size = params
                        .get("arguments")
                        .and_then(|a| a.get("size"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("large");

                    let config = load_config()?;
                    let now = Utc::now();
                    let s = evaluate(config, now);
                    let defer = should_defer(size, &s);

                    let reason = if defer {
                        "below favorable threshold"
                    } else if s.multiplier >= s.thresholds.defer_at_multiplier_below {
                        "already in favorable window"
                    } else {
                        "size not worth deferring"
                    };

                    let api_status = get_api_status_fresh();
                    let mcp_conn = get_api_connectivity_fresh();
                    let mcp_5xx = mcp_conn.as_ref().filter(|c| c.is_server_error());
                    let api_warning = if let Some(c) = mcp_5xx {
                        Some(format!("API {}", c.display()))
                    } else {
                        api_status
                            .as_ref()
                            .filter(|s| s.has_incident())
                            .map(|s| format!("API {}: {}", s.indicator, s.description))
                    };

                    let result = serde_json::json!({
                        "defer": defer,
                        "multiplier": s.multiplier,
                        "reason": reason,
                        "favorable_in_mins": s.mins_until_favorable,
                        "api_status": api_status.as_ref().map(|s| &s.indicator),
                        "api_status_warning": api_warning,
                        "api_server_error": mcp_5xx.is_some(),
                        "api_http_status": mcp_5xx.map(|c| c.http_status),
                        "online": api_status.as_ref().map(|s| s.online),
                    });

                    jsonrpc_response(
                        id,
                        serde_json::json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&result)?,
                            }]
                        }),
                    )
                }
            }

            _ => jsonrpc_error(id, -32601, "method not found"),
        };

        serde_json::to_writer(&mut stdout, &resp)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }

    Ok(())
}

fn init_claude_dir(claude_dir: &PathBuf, force: bool) -> Result<()> {
    fs::create_dir_all(claude_dir)?;

    // 1. Write windows.json
    let windows_dest = claude_dir.join("usage-windows.json");
    if windows_dest.exists() && !force {
        println!(
            "  Config exists: {} (not overwritten, use --force)",
            windows_dest.display()
        );
    } else {
        if force && windows_dest.exists() {
            println!("  Overwriting:   {}", windows_dest.display());
        } else {
            println!("  Initialized:   {}", windows_dest.display());
        }
        fs::write(&windows_dest, DEFAULT_WINDOWS_JSON)?;
    }

    // 2. Register as statusLine in settings.json (non-destructive merge)
    let settings_path = claude_dir.join("settings.json");
    let raw = if settings_path.exists() {
        fs::read_to_string(&settings_path)?
    } else {
        "{}".into()
    };
    let mut settings: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

    if settings.get("statusLine").is_none() {
        settings["statusLine"] =
            serde_json::json!({ "type": "command", "command": "claude-usage statusline" });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        println!("  statusLine registered in: {}", settings_path.display());
    } else {
        println!("  statusLine already set in: {}", settings_path.display());
    }

    // 3. Register MCP server in settings.json (non-destructive)
    let needs_mcp = settings
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .is_none_or(|obj| !obj.contains_key("claude-usage"));
    if needs_mcp {
        if !settings.get("mcpServers").is_some_and(|v| v.is_object()) {
            settings["mcpServers"] = serde_json::json!({});
        }
        settings["mcpServers"]["claude-usage"] =
            serde_json::json!({ "command": "claude-usage", "args": ["mcp"] });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        println!("  MCP server registered in: {}", settings_path.display());
    } else {
        println!("  MCP server already set in: {}", settings_path.display());
    }

    Ok(())
}

fn find_claude_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(&home) else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".claude") && e.path().is_dir()
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

fn run_init(force: bool) -> Result<()> {
    // If CLAUDE_CONFIG_DIR is set, only init that one
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        println!("{}:", dir.display());
        init_claude_dir(&dir, force)?;
        println!("\nRestart Claude Code to activate.");
        return Ok(());
    }

    // Scan for ~/.claude* directories
    let mut dirs = find_claude_dirs();
    if dirs.is_empty() {
        // No existing dirs, create ~/.claude
        let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
        dirs.push(PathBuf::from(home).join(".claude"));
    }

    for dir in &dirs {
        println!("{}:", dir.display());
        init_claude_dir(dir, force)?;
        println!();
    }

    println!("Restart Claude Code to activate.");
    Ok(())
}

const DEFAULT_WINDOWS_JSON: &str = include_str!("../config/windows.json");
