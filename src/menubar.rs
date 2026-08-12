// ---------------------------------------------------------------------------
// Menu bar integration (SwiftBar / xbar plugin format)
//
// `claude-usage menubar` prints one refresh of the menu: a compact title for
// the bar itself, then the dropdown — running loops with stage meters and
// hoverable tooltips, click-through stage submenus, per-session quit, a
// keep-awake toggle, and dashboard links. SwiftBar runs it on the cadence in
// the plugin filename (claude-usage-loops.15s.sh).
//
// Icons are SF Symbols via SwiftBar's `sfimage` param — vector-crisp on any
// display and tinted to match the menu automatically. xbar ignores the param
// and just shows text.
//
// Visibility: running loops always show; stopped loops fade out after 72h
// (they stay in the CLI and dashboard) and carry a Dismiss action to hide
// them immediately. Dismissed loops reappear if they run again.
// ---------------------------------------------------------------------------

use crate::{awake, loops};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc,
    time::{Duration, Instant},
};

const NATIVE_APP_SOURCE: &str = include_str!("../native/ClaudeUsageBar.swift");
const NATIVE_STATUS_ICON: &[u8] = include_bytes!("../native/StatusIcon-ai.svg");
const NATIVE_CODEX_ICON: &[u8] = include_bytes!("../native/ProviderIcon-codex.svg");
const NATIVE_CLAUDE_ICON: &[u8] = include_bytes!("../native/ProviderIcon-claude.svg");

// Threshold colors — one hex per level, shared by the meter lines (via
// usage_color) and the fixed-semantic lines (off-peak/peak/incident). See
// usage_color for the light/dark-legibility rationale behind the values.
const GREEN: &str = "#0a8f0a";
const AMBER: &str = "#b37400";
const RED: &str = "#d64545";

const WEEK_SECS: i64 = 604_800;

/// SwiftBar item text must not contain the param delimiter, newlines, or
/// double quotes (which would close a quoted param value).
fn clean(s: &str) -> String {
    s.replace('|', "¦").replace('\n', " ").replace('"', "'")
}

fn sf(name: &str) -> String {
    format!(" sfimage={name}")
}

fn exe() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "claude-usage".into())
}

fn dashboard_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    TcpStream::connect_timeout(
        &"127.0.0.1:4711".parse().unwrap(),
        Duration::from_millis(150),
    )
    .is_ok()
}

fn hours_since(ts: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> i64 {
    ts.and_then(loops::parse_ts)
        .map(|t| (now - t).num_hours())
        .unwrap_or(i64::MAX)
}

// Dot meters read cleanly at menu size in the system font; terminal-style
// █░ blocks smear into slabs there, and ▰▱ renders as tiny slivers.
fn meter(done: usize, total: usize) -> String {
    let total = total.max(1);
    let filled = done.min(total);
    format!("{}{}", "●".repeat(filled), "○".repeat(total - filled))
}

fn pct_meter(pct: f64) -> String {
    meter((pct / 10.0).round() as usize, 10)
}

/// Same thresholds as the CLI statusline (ctx_colored): green under 50%,
/// amber under 80%, red past that.
///
/// One hex must serve both menu appearances (SwiftBar's light,dark pairs
/// mis-detect on dark-wallpaper menu bars), and a single value tops out
/// around 3.6:1 against both a light and a dark menu — these are picked at
/// that balance point, deep enough not to wash out on white. The colored
/// SF Symbol dot doubles the cue so the state never rides on text tint alone.
fn usage_color(pct: f64) -> &'static str {
    if pct < 50.0 {
        GREEN
    } else if pct < 80.0 {
        AMBER
    } else {
        RED
    }
}

/// CodexBar-style usage block: promo window, 5h/7d rate-limit bars with pace
/// markers and reset countdowns, day/week token totals, session cost, and any
/// API incident. Everything comes from local caches the statusline keeps
/// fresh — no network from the menu's 15s cadence.
fn print_usage(now: chrono::DateTime<chrono::Utc>) {
    let mut lines: Vec<String> = vec![];

    if let Ok(cfg) = crate::load_config() {
        let s = crate::evaluate(cfg, now);
        if !s.active_windows.is_empty() {
            // "1x off-peak" reads like a bug — name the multiplier only when
            // it actually multiplies
            let mult = if s.multiplier != 1.0 {
                format!("{:.0}x ", s.multiplier)
            } else {
                Default::default()
            };
            if s.favorable {
                lines.push(format!(
                    "{mult}off-peak · ends in {} |{} sfcolor={GREEN} color={GREEN}",
                    crate::fmt_mins_opt(s.mins_until_change),
                    sf("bolt.fill")
                ));
            } else {
                let next = s
                    .active_windows
                    .iter()
                    .filter(|w| w.favorable)
                    .map(|w| w.multiplier)
                    .next()
                    .unwrap_or(2.0);
                lines.push(format!(
                    "{mult}peak · {next:.0}x in {} |{} sfcolor={AMBER} color={AMBER}",
                    crate::fmt_mins_opt(s.mins_until_favorable),
                    sf("clock")
                ));
            }
        }
    }

    let cache = crate::load_statusline_cache();
    let age_mins = cache
        .as_ref()
        .and_then(|c| loops::parse_ts(&c.updated))
        .map(|t| (now - t).num_minutes())
        .unwrap_or(i64::MAX);
    if let Some(c) = &cache {
        let now_ts = now.timestamp();
        let windows: [(&str, Option<f64>, Option<i64>); 2] = [
            ("5h", c.five_hour_pct, c.five_hour_resets_at),
            ("7d", c.seven_day_pct, c.seven_day_resets_at),
        ];
        for (label, pct, resets_at) in windows {
            let Some(pct) = pct else { continue };
            let reset = resets_at
                .filter(|&ts| ts > now_ts)
                .map(|ts| format!(" · resets {}", crate::fmt_mins(((ts - now_ts) / 60) as u32)))
                .unwrap_or_default();
            lines.push(format!(
                "{}  {}  {:.0}%{} |{} sfcolor={} color={}",
                label,
                pct_meter(pct),
                pct,
                reset,
                sf("circle.fill"),
                usage_color(pct),
                usage_color(pct)
            ));
        }
        // pace verdict off the 7d window, like CodexBar's "Pace: Behind"
        if let (Some(pct), Some(ts)) = (c.seven_day_pct, c.seven_day_resets_at) {
            if ts > now_ts {
                let elapsed = WEEK_SECS - (ts - now_ts);
                let pace = (elapsed as f64 / WEEK_SECS as f64 * 100.0).clamp(0.0, 100.0);
                let delta = pct - pace;
                let word = if delta > 1.0 { "ahead" } else { "behind" };
                if delta.abs() > 1.0 {
                    lines.push(format!("pace: {word} ({delta:+.0}%) | size=12"));
                }
            }
        }
        if age_mins <= 120 {
            // sum cost over recently-active sessions — with several Claudes
            // running, any single session's figure is misleading
            let fresh: Vec<_> = c
                .sessions
                .values()
                .filter(|s| {
                    loops::parse_ts(&s.updated)
                        .map(|t| (now - t).num_minutes() <= 120)
                        .unwrap_or(false)
                })
                .filter(|s| s.cost_usd > 0.001)
                .collect();
            if !fresh.is_empty() {
                let total: f64 = fresh.iter().map(|s| s.cost_usd).sum();
                let label = if fresh.len() == 1 {
                    "session".to_string()
                } else {
                    format!("{} sessions", fresh.len())
                };
                let rate = (fresh.len() == 1)
                    .then(|| fresh[0])
                    .filter(|s| s.duration_ms > 60_000)
                    .map(|s| {
                        format!(
                            " (${:.2}/h)",
                            s.cost_usd / (s.duration_ms as f64 / 3_600_000.0)
                        )
                    })
                    .unwrap_or_default();
                lines.push(format!("{label} ~${total:.2}{rate}"));
            }
        } else if age_mins < i64::MAX {
            lines.push(format!(
                "as of {} ago | size=12",
                crate::fmt_mins(age_mins.min(u32::MAX as i64) as u32)
            ));
        }
    }

    let state = crate::load_usage_state();
    let day = state
        .daily
        .get(&now.format("%Y-%m-%d").to_string())
        .copied()
        .unwrap_or(0);
    let week = state
        .weekly
        .get(&crate::iso_week_key(now))
        .copied()
        .unwrap_or(0);
    if day > 0 || week > 0 {
        lines.push(format!(
            "today {} · week {} tokens",
            crate::fmt_tokens(day),
            crate::fmt_tokens(week)
        ));
    }

    if let Some(api) = crate::load_cached_api_status() {
        if api.indicator != "none" && api.indicator != "unknown" {
            lines.push(format!(
                "API: {} |{} sfcolor={RED} color={RED} href=https://status.claude.com",
                clean(&api.description),
                sf("exclamationmark.triangle")
            ));
        }
    }

    if !lines.is_empty() {
        println!("Claude Usage | size=11");
        for l in lines {
            println!("{l}");
        }
        println!("---");
    }
}

pub fn run_menubar() {
    let mut cache = loops::load_scan_cache();
    let all = loops::collect_loops(&[], &mut cache);
    loops::save_scan_cache(&cache);
    let dismissed = loops::load_dismissed();
    let awake_state = awake::status();
    let now = chrono::Utc::now();
    let exe = exe();
    let up = dashboard_up();

    let ralphs: Vec<_> = all
        .iter()
        .filter(|l| l.kind == "ralph" && !loops::is_dismissed(l, &dismissed))
        .collect();
    let running: Vec<_> = ralphs.iter().filter(|l| l.running).copied().collect();
    let recent: Vec<_> = ralphs
        .iter()
        .filter(|l| !l.running && hours_since(l.updated.as_deref(), now) <= 72)
        .copied()
        .collect();
    let older = ralphs
        .len()
        .saturating_sub(running.len())
        .saturating_sub(recent.len());
    let sessions: Vec<_> = all.iter().filter(|l| l.kind != "ralph").collect();

    // ── Title ──────────────────────────────────────────────────────────────
    let mut title = match running.as_slice() {
        [] => String::new(),
        [one] => match &one.stage {
            Some(s) => format!("{}/{}", s.current, s.total),
            None => "1".into(),
        },
        many => format!("{}", many.len()),
    };
    if awake_state.is_some() {
        title.push_str(" ☕");
    }
    println!("{} |{}", title, sf("repeat"));
    println!("---");

    print_usage(now);

    // ── Ralph loops ────────────────────────────────────────────────────────
    println!("Ralph Loops | size=11");
    if running.is_empty() && recent.is_empty() {
        println!("No active loops | size=12");
    }
    for l in running.iter().chain(recent.iter()) {
        let icon = match l.state.as_str() {
            "running" => sf("repeat"),
            "completed" => sf("checkmark.circle"),
            "failed" => sf("exclamationmark.triangle"),
            _ => sf("pause.circle"),
        };
        let mut text = l.name.clone();
        if let Some(s) = &l.stage {
            text.push_str(&format!("  ·  stage {}/{}", s.current, s.total));
            if let Some(t) = &s.title {
                text.push_str(&format!(" — {t}"));
            }
        } else if !l.running {
            text.push_str(&format!("  ·  {}", l.state));
        }
        let tooltip = l
            .last_event
            .as_ref()
            .map(|e| clean(&format!("{}: {}", e.topic, e.text)))
            .unwrap_or_default();
        let action = if up {
            " href=http://127.0.0.1:4711"
        } else {
            ""
        };
        println!(
            "{} | tooltip=\"{}\"{}{}",
            clean(&text),
            tooltip.chars().take(200).collect::<String>(),
            icon,
            action
        );

        // click-through: stages, freshness, dismiss — these `--` items must
        // directly follow the name row so the submenu anchors to it
        for s in &l.stages {
            let mark = match s.status.as_str() {
                "done" => "✓",
                "current" => "▶",
                _ => "·",
            };
            let closed = s.tasks.iter().filter(|t| t.status == "closed").count();
            let tally = if s.tasks.is_empty() {
                String::new()
            } else {
                format!("  ({}/{} tasks)", closed, s.tasks.len())
            };
            println!(
                "-- {} {} {}{}",
                mark,
                s.n,
                clean(s.title.as_deref().unwrap_or("")),
                tally
            );
        }
        if let Some(u) = &l.updated {
            println!("-- updated {}", loops::ago(u, now));
        }
        if !l.running {
            println!(
                "-- Dismiss from menu | bash=\"{}\" param1=loops param2=dismiss param3=\"{}\" terminal=false refresh=true{}",
                exe,
                l.id,
                sf("eye.slash")
            );
        }

        // quiet stage meter under the row (its own item, no submenu)
        if l.running && !l.stages.is_empty() {
            let done = l.stages.iter().filter(|s| s.status == "done").count();
            let mut bar = format!(
                "{}  {}/{}",
                meter(done, l.stages.len()),
                done,
                l.stages.len()
            );
            if let Some(s) = &l.stage {
                if let (Some(a), Some(b)) = (s.step, s.step_total) {
                    bar.push_str(&format!(" · step {a}/{b}"));
                }
                if let Some(i) = l.iteration {
                    bar.push_str(&format!(" · it {i}"));
                }
            }
            println!("{bar} | size=12");
        }
    }
    if older > 0 {
        let action = if up {
            " href=http://127.0.0.1:4711"
        } else {
            ""
        };
        println!("{older} older in the dashboard | size=12{action}");
    }

    // ── Sessions ───────────────────────────────────────────────────────────
    if !sessions.is_empty() {
        println!("---");
        println!("Claude Sessions | size=11");
        for l in &sessions {
            let icon = match l.state.as_str() {
                "busy" => sf("ellipsis.bubble"),
                "shell" => sf("terminal"),
                _ => sf("bubble.left"),
            };
            let goal = l
                .goal
                .as_ref()
                .map(|g| format!("  ⌖ {}", g.text.chars().take(60).collect::<String>()))
                .unwrap_or_default();
            // label the preview clearly — it's the session's own chatter, and
            // an unlabeled quote about e.g. ralph work reads like a claim
            // about this session's state
            let tip = l
                .recent
                .last()
                .map(|m| format!("last message ({}): {}", m.role, m.text))
                .unwrap_or_else(|| l.dir.clone());
            println!(
                "{}  ·  {}{} | tooltip=\"{}\"{}",
                clean(&l.name),
                l.state,
                clean(&goal),
                clean(&tip).chars().take(200).collect::<String>(),
                icon
            );
            if let Some(u) = &l.updated {
                println!("-- {} · {}", loops::ago(u, now), clean(&l.dir));
            }
            if let Some(pid) = l.pid {
                println!(
                    "-- Quit session | bash=\"{}\" param1=loops param2=quit param3={} terminal=false refresh=true{}",
                    exe,
                    pid,
                    sf("power")
                );
                println!("---- transcript is saved; `claude --resume` restores it | size=12");
            }
        }
    }

    // ── Keep awake ─────────────────────────────────────────────────────────
    println!("---");
    match &awake_state {
        Some(s) => {
            let lid = if s.lid { " · lid-closed mode" } else { "" };
            println!(
                "Awake{} — click to let it sleep | bash=\"{}\" param1=awake param2=off terminal=false refresh=true{}",
                lid,
                exe,
                sf("cup.and.saucer.fill")
            );
        }
        None => {
            println!(
                "Keep awake | bash=\"{}\" param1=awake param2=on terminal=false refresh=true{}",
                exe,
                sf("moon.zzz")
            );
            println!(
                "-- with closed-lid support (sudo) | bash=\"{exe}\" param1=awake param2=on param3=--lid terminal=true refresh=true"
            );
        }
    }

    // ── Dashboard ──────────────────────────────────────────────────────────
    if up {
        println!("Open dashboard | href=http://127.0.0.1:4711{}", sf("gauge"));
    } else {
        println!(
            "Start dashboard | bash=\"{}\" param1=loops param2=--serve param3=--open terminal=false refresh=true{}",
            exe,
            sf("gauge")
        );
    }
    println!("Refresh |{} refresh=true", sf("arrow.clockwise"));
}

// ---------------------------------------------------------------------------
// Native menu snapshot
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct NativeMenuSnapshot {
    status_text: String,
    updated_at: String,
    usage: NativeUsageSnapshot,
    codex: NativeCodexUsageSnapshot,
    loops: Vec<NativeLoopSummary>,
    sessions: Vec<NativeSessionSummary>,
    older_loop_count: usize,
    awake: Option<NativeAwakeSummary>,
    dashboard_running: bool,
}

#[derive(Serialize)]
struct NativeUsageSnapshot {
    model: Option<String>,
    updated_label: String,
    five_hour: Option<NativeUsageMeter>,
    seven_day: Option<NativeUsageMeter>,
    context: Option<NativeUsageMeter>,
    pace: Option<NativePaceSummary>,
    active_cost_usd: f64,
    active_sessions: usize,
    hourly_cost_usd: Option<f64>,
    today_tokens: u64,
    week_tokens: u64,
    promo_title: Option<String>,
    promo_detail: Option<String>,
    favorable: bool,
    multiplier: f64,
    api_indicator: Option<String>,
    api_description: Option<String>,
}

#[derive(Serialize)]
struct NativeUsageMeter {
    percent: f64,
    reset_label: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct NativeCodexUsageSnapshot {
    available: bool,
    updated_label: String,
    plan: Option<String>,
    windows: Vec<NativeCodexUsageWindow>,
    credits_remaining: Option<f64>,
    credits_unlimited: bool,
    stale: bool,
    error: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct NativeCodexUsageWindow {
    id: String,
    title: String,
    percent: f64,
    reset_label: Option<String>,
    additional: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRpcRateLimitsResult {
    rate_limits: CodexRpcRateLimit,
    #[serde(default, alias = "rate_limits_by_limit_id")]
    rate_limits_by_limit_id: HashMap<String, CodexRpcRateLimit>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRpcRateLimit {
    #[serde(default, alias = "limit_id")]
    limit_id: Option<String>,
    #[serde(default, alias = "limit_name")]
    limit_name: Option<String>,
    #[serde(default)]
    primary: Option<CodexRpcRateLimitWindow>,
    #[serde(default)]
    secondary: Option<CodexRpcRateLimitWindow>,
    #[serde(default)]
    credits: Option<CodexRpcCredits>,
    #[serde(default, alias = "plan_type")]
    plan_type: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRpcRateLimitWindow {
    used_percent: f64,
    #[serde(default)]
    window_duration_mins: Option<i64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRpcCredits {
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    balance: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CodexRpcEnvelope {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<CodexRpcError>,
}

#[derive(Deserialize)]
struct CodexRpcError {
    message: String,
}

#[derive(Deserialize, Serialize)]
struct NativeCodexUsageCache {
    fetched_at: i64,
    snapshot: NativeCodexUsageSnapshot,
}

#[derive(Serialize)]
struct NativePaceSummary {
    label: String,
    delta_percent: f64,
}

#[derive(Serialize)]
struct NativeLoopSummary {
    id: String,
    name: String,
    state: String,
    running: bool,
    stage_current: Option<u32>,
    stage_total: Option<u32>,
    stage_title: Option<String>,
    stage_done: usize,
    stage_count: usize,
    task_done: usize,
    task_count: usize,
    iteration: Option<u64>,
    cost_usd: Option<f64>,
    updated_label: String,
    last_event: Option<String>,
}

#[derive(Serialize)]
struct NativeSessionSummary {
    id: String,
    name: String,
    state: String,
    pid: Option<u32>,
    goal: Option<String>,
    updated_label: String,
    directory: String,
    last_message: Option<String>,
}

#[derive(Serialize)]
struct NativeAwakeSummary {
    lid: bool,
    started_label: String,
    until_label: Option<String>,
}

fn reset_label(resets_at: Option<i64>, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let seconds = resets_at?.checked_sub(now.timestamp())?;
    (seconds > 0).then(|| format!("Resets in {}", crate::fmt_mins((seconds / 60) as u32)))
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max).collect::<String>())
    }
}

fn codex_binary() -> Option<PathBuf> {
    fn usable(path: &Path) -> bool {
        path.is_file()
    }

    if let Some(path) = std::env::var_os("CLAUDE_USAGE_CODEX_BIN").map(PathBuf::from) {
        if usable(&path) {
            return Some(path);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("codex");
            if usable(&candidate) {
                return Some(candidate);
            }
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut fixed = vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    if let Some(home) = &home {
        fixed.extend([
            home.join(".cargo/bin/codex"),
            home.join(".local/bin/codex"),
            home.join(".volta/bin/codex"),
        ]);
    }
    if let Some(found) = fixed.into_iter().find(|candidate| usable(candidate)) {
        return Some(found);
    }

    // GUI apps do not inherit an interactive shell's nvm PATH. Resolve the
    // newest locally installed Node toolchain directly, then prepend its bin
    // directory when launching so the codex script can also find `node`.
    let versions = home?.join(".nvm/versions/node");
    let mut candidates: Vec<_> = fs::read_dir(versions)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("bin/codex"))
        .filter(|candidate| usable(candidate))
        .collect();
    candidates.sort_by_key(|candidate| {
        fs::metadata(candidate)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn codex_rpc_reply(
    receiver: &mpsc::Receiver<String>,
    wanted_id: i64,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Codex app-server timed out"));
        }
        let line = receiver
            .recv_timeout(remaining)
            .map_err(|_| anyhow!("Codex app-server timed out"))?;
        if line.len() > 1_048_576 {
            return Err(anyhow!("Codex app-server returned an oversized response"));
        }
        let envelope: CodexRpcEnvelope = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let id = envelope.id.as_ref().and_then(|value| match value {
            serde_json::Value::Number(number) => number.as_i64(),
            serde_json::Value::String(value) => value.parse().ok(),
            _ => None,
        });
        if id != Some(wanted_id) {
            // Codex can publish notifications on the same stdout stream.
            continue;
        }
        if let Some(error) = envelope.error {
            return Err(anyhow!("Codex app-server: {}", error.message));
        }
        return envelope
            .result
            .ok_or_else(|| anyhow!("Codex app-server response was missing its result"));
    }
}

fn fetch_codex_rate_limits() -> Result<CodexRpcRateLimitsResult> {
    let binary = codex_binary().ok_or_else(|| anyhow!("Codex CLI is not installed"))?;
    let mut command = std::process::Command::new(&binary);
    command
        .args(["-s", "read-only", "-a", "untrusted", "app-server"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(parent) = binary.parent() {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![parent.to_path_buf()];
        paths.extend(std::env::split_paths(&inherited));
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("could not launch {}", binary.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("could not open Codex app-server input"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("could not open Codex app-server output"))?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let result = (|| -> Result<CodexRpcRateLimitsResult> {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "claude-usage",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            })
        )?;
        stdin.flush()?;
        let _ = codex_rpc_reply(&receiver, 1, Duration::from_secs(8))?;

        writeln!(
            stdin,
            "{}",
            serde_json::json!({"method": "initialized", "params": {}})
        )?;
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "id": 2,
                "method": "account/rateLimits/read",
                "params": {}
            })
        )?;
        stdin.flush()?;
        let value = codex_rpc_reply(&receiver, 2, Duration::from_secs(5))?;
        serde_json::from_value(value).context("could not decode Codex rate limits")
    })();

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

fn codex_window_title(minutes: Option<i64>, fallback: &str) -> String {
    match minutes {
        Some(300) => "Session".to_string(),
        Some(10_080) => "Weekly".to_string(),
        Some(43_200 | 44_640) => "Monthly".to_string(),
        Some(value) if value > 0 && value % 1_440 == 0 => {
            format!("{} day", value / 1_440) + if value == 1_440 { "" } else { "s" }
        }
        Some(value) if value > 0 && value % 60 == 0 => format!("{}h", value / 60),
        _ => fallback.to_string(),
    }
}

fn codex_limit_name(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("spark") {
        "Codex Spark".to_string()
    } else {
        value.replace('-', " ")
    }
}

fn native_codex_window(
    id: String,
    title: String,
    value: &CodexRpcRateLimitWindow,
    additional: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> NativeCodexUsageWindow {
    NativeCodexUsageWindow {
        id,
        title,
        percent: value.used_percent.clamp(0.0, 100.0),
        reset_label: reset_label(value.resets_at, now),
        additional,
    }
}

fn codex_credits(value: Option<&CodexRpcCredits>) -> (Option<f64>, bool) {
    let Some(value) = value else {
        return (None, false);
    };
    let balance = value.balance.as_ref().and_then(|balance| match balance {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => value.parse().ok(),
        _ => None,
    });
    (balance, value.unlimited)
}

fn native_codex_from_rpc(
    response: CodexRpcRateLimitsResult,
    now: chrono::DateTime<chrono::Utc>,
) -> NativeCodexUsageSnapshot {
    let main_id = response
        .rate_limits
        .limit_id
        .as_deref()
        .unwrap_or("codex")
        .to_string();
    let mut windows = Vec::new();
    if let Some(primary) = &response.rate_limits.primary {
        windows.push(native_codex_window(
            format!("{main_id}-primary"),
            codex_window_title(primary.window_duration_mins, "Primary"),
            primary,
            false,
            now,
        ));
    }
    if let Some(secondary) = &response.rate_limits.secondary {
        windows.push(native_codex_window(
            format!("{main_id}-secondary"),
            codex_window_title(secondary.window_duration_mins, "Secondary"),
            secondary,
            false,
            now,
        ));
    }

    let mut additional: Vec<_> = response
        .rate_limits_by_limit_id
        .iter()
        .filter(|(id, limit)| id.as_str() != main_id && limit.limit_name.is_some())
        .collect();
    additional.sort_by_key(|(id, _)| *id);
    for (id, limit) in additional {
        let name = codex_limit_name(limit.limit_name.as_deref().unwrap_or(id));
        if let Some(primary) = &limit.primary {
            windows.push(native_codex_window(
                format!("{id}-primary"),
                name.clone(),
                primary,
                true,
                now,
            ));
        }
        if let Some(secondary) = &limit.secondary {
            windows.push(native_codex_window(
                format!("{id}-secondary"),
                format!(
                    "{} · {}",
                    name,
                    codex_window_title(secondary.window_duration_mins, "Secondary")
                ),
                secondary,
                true,
                now,
            ));
        }
    }

    let (credits_remaining, credits_unlimited) =
        codex_credits(response.rate_limits.credits.as_ref());
    NativeCodexUsageSnapshot {
        available: !windows.is_empty()
            || response.rate_limits.plan_type.is_some()
            || credits_remaining.is_some()
            || credits_unlimited,
        updated_label: "Updated just now".to_string(),
        plan: response.rate_limits.plan_type,
        windows,
        credits_remaining,
        credits_unlimited,
        stale: false,
        error: None,
    }
}

fn codex_cache_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".config/claude-usage/codex-menubar-cache.json"),
    )
}

fn load_codex_cache() -> Option<NativeCodexUsageCache> {
    let data = fs::read(codex_cache_path()?).ok()?;
    serde_json::from_slice(&data).ok()
}

fn save_codex_cache(snapshot: &NativeCodexUsageSnapshot, now: chrono::DateTime<chrono::Utc>) {
    let Some(path) = codex_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let cache = NativeCodexUsageCache {
        fetched_at: now.timestamp(),
        snapshot: snapshot.clone(),
    };
    let Ok(data) = serde_json::to_vec(&cache) else {
        return;
    };
    let temporary = path.with_extension("json.tmp");
    if fs::write(&temporary, data).is_ok() {
        let _ = fs::rename(temporary, path);
    }
}

fn collect_native_codex_usage(now: chrono::DateTime<chrono::Utc>) -> NativeCodexUsageSnapshot {
    match fetch_codex_rate_limits().map(|response| native_codex_from_rpc(response, now)) {
        Ok(snapshot) => {
            save_codex_cache(&snapshot, now);
            snapshot
        }
        Err(error) => {
            if let Some(mut cached) = load_codex_cache() {
                let fetched = chrono::DateTime::from_timestamp(cached.fetched_at, 0)
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
                cached.snapshot.updated_label = format!(
                    "Updated {} · refresh failed",
                    loops::ago(&fetched.to_rfc3339(), now)
                );
                cached.snapshot.stale = true;
                cached.snapshot.error = Some(error.to_string());
                return cached.snapshot;
            }
            NativeCodexUsageSnapshot {
                available: false,
                updated_label: "Codex unavailable".to_string(),
                plan: None,
                windows: Vec::new(),
                credits_remaining: None,
                credits_unlimited: false,
                stale: false,
                error: Some(error.to_string()),
            }
        }
    }
}

fn collect_native_usage(now: chrono::DateTime<chrono::Utc>) -> NativeUsageSnapshot {
    let (promo_title, promo_detail, favorable, multiplier) = crate::load_config()
        .ok()
        .map(|config| crate::evaluate(config, now))
        .map(|status| {
            if status.active_windows.is_empty() {
                return (None, None, status.favorable, status.multiplier);
            }
            let prefix = if status.multiplier != 1.0 {
                format!("{:.0}× ", status.multiplier)
            } else {
                String::new()
            };
            if status.favorable {
                (
                    Some(format!("{}off-peak", prefix)),
                    Some(format!(
                        "Ends in {}",
                        crate::fmt_mins_opt(status.mins_until_change)
                    )),
                    status.favorable,
                    status.multiplier,
                )
            } else {
                (
                    Some(format!("{}peak", prefix)),
                    status
                        .mins_until_favorable
                        .map(|mins| format!("Off-peak in {}", crate::fmt_mins(mins))),
                    status.favorable,
                    status.multiplier,
                )
            }
        })
        .unwrap_or((None, None, false, 1.0));

    let cache = crate::load_statusline_cache();
    let updated_label = cache
        .as_ref()
        .and_then(|value| loops::parse_ts(&value.updated))
        .map(|_| {
            format!(
                "Updated {}",
                loops::ago(
                    cache
                        .as_ref()
                        .map(|value| value.updated.as_str())
                        .unwrap_or(""),
                    now
                )
            )
        })
        .unwrap_or_else(|| "Waiting for Claude Code".to_string());

    let five_hour = cache.as_ref().and_then(|value| {
        value.five_hour_pct.map(|percent| NativeUsageMeter {
            percent: percent.clamp(0.0, 100.0),
            reset_label: reset_label(value.five_hour_resets_at, now),
        })
    });
    let seven_day = cache.as_ref().and_then(|value| {
        value.seven_day_pct.map(|percent| NativeUsageMeter {
            percent: percent.clamp(0.0, 100.0),
            reset_label: reset_label(value.seven_day_resets_at, now),
        })
    });
    let context = cache.as_ref().and_then(|value| {
        value.context_pct.map(|percent| NativeUsageMeter {
            percent: percent.clamp(0.0, 100.0),
            reset_label: None,
        })
    });
    let pace = cache.as_ref().and_then(|value| {
        let percent = value.seven_day_pct?;
        let resets_at = value.seven_day_resets_at?;
        if resets_at <= now.timestamp() {
            return None;
        }
        let elapsed = 604_800 - (resets_at - now.timestamp());
        let expected = (elapsed as f64 / 604_800.0 * 100.0).clamp(0.0, 100.0);
        let delta = percent - expected;
        (delta.abs() > 1.0).then(|| NativePaceSummary {
            label: if delta > 0.0 { "Ahead" } else { "Behind" }.to_string(),
            delta_percent: delta,
        })
    });

    let fresh_sessions: Vec<_> = cache
        .as_ref()
        .map(|value| {
            value
                .sessions
                .values()
                .filter(|session| {
                    loops::parse_ts(&session.updated)
                        .map(|updated| (now - updated).num_minutes() <= 120)
                        .unwrap_or(false)
                })
                .filter(|session| session.cost_usd > 0.001)
                .collect()
        })
        .unwrap_or_default();
    let active_cost_usd = fresh_sessions.iter().map(|session| session.cost_usd).sum();
    let hourly_cost_usd = (fresh_sessions.len() == 1)
        .then(|| fresh_sessions[0])
        .filter(|session| session.duration_ms > 60_000)
        .map(|session| session.cost_usd / (session.duration_ms as f64 / 3_600_000.0));

    let state = crate::load_usage_state();
    let today_tokens = state
        .daily
        .get(&now.format("%Y-%m-%d").to_string())
        .copied()
        .unwrap_or(0);
    let week_tokens = state
        .weekly
        .get(&crate::iso_week_key(now))
        .copied()
        .unwrap_or(0);
    let api = crate::load_cached_api_status();

    NativeUsageSnapshot {
        model: cache.as_ref().and_then(|value| value.model.clone()),
        updated_label,
        five_hour,
        seven_day,
        context,
        pace,
        active_cost_usd,
        active_sessions: fresh_sessions.len(),
        hourly_cost_usd,
        today_tokens,
        week_tokens,
        promo_title,
        promo_detail,
        favorable,
        multiplier,
        api_indicator: api.as_ref().map(|value| value.indicator.clone()),
        api_description: api.map(|value| value.description),
    }
}

fn native_loop_summary(
    value: &loops::LoopInfo,
    now: chrono::DateTime<chrono::Utc>,
) -> NativeLoopSummary {
    let stage_done = value
        .stages
        .iter()
        .filter(|stage| stage.status == "done")
        .count();
    let task_count = value.stages.iter().map(|stage| stage.tasks.len()).sum();
    let task_done = value
        .stages
        .iter()
        .flat_map(|stage| &stage.tasks)
        .filter(|task| task.status == "closed")
        .count();
    NativeLoopSummary {
        id: value.id.clone(),
        name: value.name.clone(),
        state: value.state.clone(),
        running: value.running,
        stage_current: value.stage.as_ref().map(|stage| stage.current),
        stage_total: value.stage.as_ref().map(|stage| stage.total),
        stage_title: value.stage.as_ref().and_then(|stage| stage.title.clone()),
        stage_done,
        stage_count: value.stages.len(),
        task_done,
        task_count,
        iteration: value.iteration,
        cost_usd: value.cost_usd,
        updated_label: value
            .updated
            .as_deref()
            .map(|updated| loops::ago(updated, now))
            .unwrap_or_else(|| "Unknown".to_string()),
        last_event: value
            .last_event
            .as_ref()
            .map(|event| truncate_chars(&format!("{}: {}", event.topic, event.text), 120)),
    }
}

fn native_session_summary(
    value: &loops::LoopInfo,
    now: chrono::DateTime<chrono::Utc>,
) -> NativeSessionSummary {
    NativeSessionSummary {
        id: value.id.clone(),
        name: value.name.clone(),
        state: value.state.clone(),
        pid: value.pid,
        goal: value
            .goal
            .as_ref()
            .map(|goal| truncate_chars(&goal.text, 140)),
        updated_label: value
            .updated
            .as_deref()
            .map(|updated| loops::ago(updated, now))
            .unwrap_or_else(|| "Unknown".to_string()),
        directory: value.dir.clone(),
        last_message: value
            .recent
            .last()
            .map(|message| truncate_chars(&message.text, 160)),
    }
}

fn collect_native_snapshot() -> NativeMenuSnapshot {
    let now = chrono::Utc::now();
    let mut cache = loops::load_scan_cache();
    let all = loops::collect_loops(&[], &mut cache);
    loops::save_scan_cache(&cache);
    let dismissed = loops::load_dismissed();

    let mut visible_loops: Vec<_> = all
        .iter()
        .filter(|value| value.kind == "ralph" && !loops::is_dismissed(value, &dismissed))
        .filter(|value| value.running || hours_since(value.updated.as_deref(), now) <= 72)
        .collect();
    visible_loops.sort_by(|left, right| {
        right
            .running
            .cmp(&left.running)
            .then_with(|| right.updated.cmp(&left.updated))
    });
    let older_loop_count = all
        .iter()
        .filter(|value| value.kind == "ralph" && !loops::is_dismissed(value, &dismissed))
        .filter(|value| !value.running && hours_since(value.updated.as_deref(), now) > 72)
        .count();

    let mut sessions: Vec<_> = all.iter().filter(|value| value.kind != "ralph").collect();
    sessions.sort_by(|left, right| {
        let left_busy = left.state == "busy";
        let right_busy = right.state == "busy";
        right_busy
            .cmp(&left_busy)
            .then_with(|| right.updated.cmp(&left.updated))
    });

    let awake = awake::status();
    let status_text = match visible_loops
        .iter()
        .filter(|value| value.running)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => String::new(),
        [one] => one
            .stage
            .as_ref()
            .map(|stage| format!("{}/{}", stage.current, stage.total))
            .unwrap_or_else(|| "1".to_string()),
        running => running.len().to_string(),
    };
    let awake_summary = awake.map(|state| NativeAwakeSummary {
        lid: state.lid,
        started_label: loops::ago(&state.started, now),
        until_label: state
            .until
            .as_deref()
            .and_then(loops::parse_ts)
            .filter(|until| *until > now)
            .map(|until| {
                format!(
                    "{} left",
                    crate::fmt_mins(((until - now).num_seconds() / 60).max(0) as u32)
                )
            }),
    });

    NativeMenuSnapshot {
        status_text,
        updated_at: now.to_rfc3339(),
        usage: collect_native_usage(now),
        codex: collect_native_codex_usage(now),
        loops: visible_loops
            .into_iter()
            .map(|value| native_loop_summary(value, now))
            .collect(),
        sessions: sessions
            .into_iter()
            .map(|value| native_session_summary(value, now))
            .collect(),
        older_loop_count,
        awake: awake_summary,
        dashboard_running: dashboard_up(),
    }
}

pub fn run_native_json() -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&collect_native_snapshot())?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Native app + SwiftBar installation
// ---------------------------------------------------------------------------

fn configured_swiftbar_plugin_dir() -> Option<PathBuf> {
    let read = std::process::Command::new("defaults")
        .args(["read", "com.ameba.SwiftBar", "PluginDirectory"])
        .output()
        .ok()?;
    let dir = String::from_utf8_lossy(&read.stdout).trim().to_string();
    (read.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}

fn swiftbar_plugin_dir() -> Result<PathBuf> {
    // honor an existing SwiftBar plugin directory; register one if unset
    if let Some(dir) = configured_swiftbar_plugin_dir() {
        return Ok(dir);
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".config/swiftbar");
    fs::create_dir_all(&dir)?;
    let ok = std::process::Command::new("defaults")
        .args([
            "write",
            "com.ameba.SwiftBar",
            "PluginDirectory",
            "-string",
            &dir.display().to_string(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("could not register SwiftBar plugin directory"));
    }
    Ok(dir)
}

fn disable_legacy_swiftbar_plugin(home: &str) -> Result<Option<PathBuf>> {
    let mut plugin_dirs = vec![PathBuf::from(home).join(".config/swiftbar")];
    if let Some(configured) = configured_swiftbar_plugin_dir() {
        plugin_dirs.push(configured);
    }
    plugin_dirs.sort();
    plugin_dirs.dedup();

    for dir in plugin_dirs {
        let plugin = dir.join("claude-usage-loops.15s.sh");
        if !plugin.is_file() {
            continue;
        }
        let disabled_dir = PathBuf::from(home)
            .join(".config/claude-usage")
            .join("disabled-swiftbar-plugins");
        fs::create_dir_all(&disabled_dir)?;
        let mut destination = disabled_dir.join("claude-usage-loops.15s.sh");
        if destination.exists() {
            destination = disabled_dir.join(format!(
                "claude-usage-loops.{}.15s.sh",
                chrono::Utc::now().timestamp()
            ));
        }
        fs::rename(&plugin, &destination).with_context(|| {
            format!(
                "could not disable legacy SwiftBar plugin {}",
                plugin.display()
            )
        })?;
        return Ok(Some(destination));
    }
    Ok(None)
}

pub fn install_swiftbar_plugin() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!("menu bar install is macOS-only (SwiftBar)"));
    }
    let dir = swiftbar_plugin_dir()?;
    let plugin = dir.join("claude-usage-loops.15s.sh");
    // prefer whatever claude-usage is on PATH (survives reinstalls), falling
    // back to the binary that ran --install
    let shim = format!(
        "#!/bin/sh\nPATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v claude-usage >/dev/null 2>&1; then exec claude-usage menubar; fi\nexec \"{}\" menubar\n",
        exe()
    );
    fs::write(&plugin, shim)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&plugin, fs::Permissions::from_mode(0o755))?;
    }
    println!("  Plugin installed: {}", plugin.display());
    println!("  Refresh cadence:  15s (rename the file to change)");
    println!("  If SwiftBar isn't installed yet:  brew install --cask swiftbar");
    println!("  Then launch it:                   open -a SwiftBar");
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn install_native_app() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!("the native menu bar app is macOS-only"));
    }

    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    let app = PathBuf::from(&home)
        .join("Applications")
        .join("Claude Usage.app");
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos).with_context(|| format!("could not create {}", macos.display()))?;
    fs::create_dir_all(&resources)
        .with_context(|| format!("could not create {}", resources.display()))?;

    let _ = std::process::Command::new("pkill")
        .args(["-x", "ClaudeUsageBar"])
        .status();

    let source_dir = std::env::temp_dir().join(format!(
        "claude-usage-menubar-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("ClaudeUsageBar.swift");
    fs::write(&source, NATIVE_APP_SOURCE)?;

    let executable = macos.join("ClaudeUsageBar");
    let output = std::process::Command::new("xcrun")
        .args([
            "swiftc",
            "-Osize",
            "-parse-as-library",
            "-framework",
            "AppKit",
            "-framework",
            "SwiftUI",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .env("MACOSX_DEPLOYMENT_TARGET", "14.0")
        .output()
        .context("could not run xcrun swiftc; install Xcode Command Line Tools")?;
    let _ = fs::remove_dir_all(&source_dir);
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "native menu app compilation failed{}{}",
            if details.is_empty() { "" } else { ":\n" },
            details
        ));
    }

    let cli_path = std::env::current_exe()
        .context("could not resolve the claude-usage executable")?
        .canonicalize()
        .unwrap_or_else(|_| exe().into());
    let info = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>Claude Usage</string>
    <key>CFBundleExecutable</key><string>ClaudeUsageBar</string>
    <key>CFBundleIdentifier</key><string>com.claudeusage.menubar</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>Claude Usage</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
    <key>ClaudeUsageCLIPath</key><string>{cli_path}</string>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION"),
        cli_path = xml_escape(&cli_path.display().to_string()),
    );
    fs::write(contents.join("Info.plist"), info)?;
    fs::write(resources.join("StatusIcon-ai.svg"), NATIVE_STATUS_ICON)?;
    fs::write(resources.join("ProviderIcon-codex.svg"), NATIVE_CODEX_ICON)?;
    fs::write(
        resources.join("ProviderIcon-claude.svg"),
        NATIVE_CLAUDE_ICON,
    )?;

    let _ = std::process::Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&app)
        .status();
    let disabled_plugin = disable_legacy_swiftbar_plugin(&home)?;
    if disabled_plugin.is_some() {
        let _ = std::process::Command::new("pkill")
            .args(["-x", "SwiftBar"])
            .status();
        let _ = std::process::Command::new("open")
            .args(["-a", "SwiftBar"])
            .status();
    }
    let launched = std::process::Command::new("open")
        .arg("-n")
        .arg(&app)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !launched {
        return Err(anyhow!("installed the app but could not launch it"));
    }

    println!("  Native menu app installed: {}", app.display());
    if let Some(path) = disabled_plugin {
        println!("  Legacy SwiftBar plugin disabled: {}", path.display());
    }
    println!("  It is running now; reinstall after moving the claude-usage binary.");
    println!("  SwiftBar fallback: claude-usage menubar --install-swiftbar");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn reset_label_ignores_expired_windows() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        assert_eq!(
            reset_label(Some(now.timestamp() + 5_400), now).as_deref(),
            Some("Resets in 1h 30m")
        );
        assert_eq!(reset_label(Some(now.timestamp()), now), None);
        assert_eq!(reset_label(Some(now.timestamp() - 60), now), None);
    }

    #[test]
    fn snapshot_text_truncation_preserves_unicode_boundaries() {
        assert_eq!(truncate_chars("Claude", 10), "Claude");
        assert_eq!(truncate_chars("agent 🧠 loop", 7), "agent 🧠…");
    }

    #[test]
    fn installer_plist_values_are_xml_escaped() {
        assert_eq!(
            xml_escape("A&B <menu> \"app\""),
            "A&amp;B &lt;menu&gt; &quot;app&quot;"
        );
    }

    #[test]
    fn codex_rpc_snapshot_maps_provider_windows_and_credits() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        let response: CodexRpcRateLimitsResult = serde_json::from_value(serde_json::json!({
            "rateLimits": {
                "limitId": "codex",
                "planType": "pro",
                "primary": {
                    "usedPercent": 8,
                    "windowDurationMins": 10080,
                    "resetsAt": now.timestamp() + 3600
                },
                "credits": {"unlimited": false, "balance": "12.5"}
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "primary": {"usedPercent": 8, "windowDurationMins": 10080}
                },
                "codex_bengalfox": {
                    "limitId": "codex_bengalfox",
                    "limitName": "GPT-5.3-Codex-Spark",
                    "primary": {"usedPercent": 17, "windowDurationMins": 10080}
                }
            }
        }))
        .unwrap();

        let snapshot = native_codex_from_rpc(response, now);
        assert!(snapshot.available);
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.credits_remaining, Some(12.5));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].title, "Weekly");
        assert_eq!(
            snapshot.windows[0].reset_label.as_deref(),
            Some("Resets in 1h 00m")
        );
        assert_eq!(snapshot.windows[1].title, "Codex Spark");
        assert!(snapshot.windows[1].additional);
    }

    #[test]
    fn codex_window_titles_follow_reported_duration() {
        assert_eq!(codex_window_title(Some(300), "Primary"), "Session");
        assert_eq!(codex_window_title(Some(10_080), "Primary"), "Weekly");
        assert_eq!(codex_window_title(Some(43_200), "Primary"), "Monthly");
        assert_eq!(codex_window_title(Some(720), "Primary"), "12h");
    }
}
