use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, NaiveTime, Utc, Weekday};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io::Read, path::PathBuf, thread, time::Duration};

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
    current_usage: Option<CurrentUsage>,
    cost: Option<CostInfo>,
}

#[derive(Debug, Deserialize)]
struct StatuslineModel {
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct ContextWindowInfo {
    used_percentage: Option<f64>,
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

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "claude-usage",
    about = "Claude usage window optimizer: status bar, token tracking, defer logic",
    version,
    long_version = env!("CARGO_PKG_VERSION"),
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
    /// Block until a favorable window opens
    Wait,
    /// Check if a task should be deferred (small|medium|large|xl)
    Defer {
        #[arg(default_value = "large")]
        size: String,
    },
    /// Write windows.json to ~/.claude/ and register statusLine in settings.json
    Init,
}

fn main() -> Result<()> {
    let cmd = Cli::parse().command.unwrap_or(Cmd::Status);

    if let Cmd::Init = cmd {
        return run_init();
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
        Cmd::Wait => run_wait(&s),
        Cmd::Defer { size } => run_defer(&s, &size),
        Cmd::Init => unreachable!(),
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
        .current_usage
        .as_ref()
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

    // ── Line 1: promo/peak status (omitted when no window is active) ──────────
    if !s.active_windows.is_empty() {
        if s.favorable {
            println!(
                "{} {}  ends in {}",
                ansi("32;1", &format!("⚡{:.0}x", s.multiplier)),
                ansi("32;1", "OFF-PEAK"),
                fmt_mins_opt(s.mins_until_change)
            );
        } else {
            println!(
                "{} {}  {:.0}x in {}",
                ansi("33;1", &format!("·{:.0}x", s.multiplier)),
                ansi("33;1", "PEAK"),
                s.active_windows
                    .iter()
                    .filter(|w| w.favorable)
                    .map(|w| w.multiplier)
                    .next()
                    .unwrap_or(2.0),
                fmt_mins_opt(s.mins_until_favorable)
            );
        }
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
        if pct > 0.0 {
            let bar = ctx_bar(pct, 8);
            let text = format!("{} {:.0}%", bar, pct);
            parts.push(ctx_colored(pct, &text));
        }
    }

    if session_total > 0 {
        parts.push(format!("sess {}", fmt_tokens(session_total)));
    }
    if day_total > 0 {
        parts.push(format!("day {}", fmt_tokens(day_total)));
    }
    if week_total > 0 {
        parts.push(format!("wk {}", fmt_tokens(week_total)));
    }

    if let Some(cost) = cc.cost.as_ref().and_then(|c| c.total_cost_usd) {
        if cost > 0.001 {
            parts.push(ansi("90", &format!("${:.3}", cost)));
        }
    }

    if !parts.is_empty() {
        println!("{}", parts.join(" │ "));
    }
}

// ---------------------------------------------------------------------------
// Other commands (unchanged)
// ---------------------------------------------------------------------------

fn run_status(s: &Status) {
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
        println!(
            "   Promo: {} (ends in {})",
            w.window.label,
            fmt_mins(w.promo_ends_in)
        );
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
        active_windows: Vec<WinOut>,
        inactive_windows: Vec<WinOut>,
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
    println!(
        "{}",
        serde_json::to_string_pretty(&Out {
            now: s.now.to_rfc3339(),
            multiplier: s.multiplier,
            favorable: s.favorable,
            mins_until_favorable: s.mins_until_favorable,
            mins_until_change: s.mins_until_change,
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

fn run_init() -> Result<()> {
    let claude_dir =
        claude_config_dir().ok_or_else(|| anyhow!("neither CLAUDE_CONFIG_DIR nor HOME is set"))?;
    fs::create_dir_all(&claude_dir)?;

    // 1. Write windows.json
    let windows_dest = claude_dir.join("usage-windows.json");
    if windows_dest.exists() {
        println!(
            "Config exists: {} (not overwritten)",
            windows_dest.display()
        );
    } else {
        fs::write(&windows_dest, DEFAULT_WINDOWS_JSON)?;
        println!("Initialized:   {}", windows_dest.display());
    }

    // 2. Register as statusLine in ~/.claude/settings.json (non-destructive merge)
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
        println!("statusLine registered in: {}", settings_path.display());
    } else {
        println!("statusLine already set in: {}", settings_path.display());
    }

    println!("\nRestart Claude Code to activate the status bar.");
    println!("Edit {} to add future promos.", windows_dest.display());
    Ok(())
}

const DEFAULT_WINDOWS_JSON: &str = include_str!("../config/windows.json");
