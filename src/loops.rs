// ---------------------------------------------------------------------------
// Loop discovery and parsing
//
// Two loop families:
//   1. Ralph orchestrator loops — a `.ralph/` dir in a project root holding
//      loop.lock (pid/started/prompt), current-events (relative path to the
//      active events-*.jsonl), history.jsonl (loop_started/loop_completed),
//      stageN-prompt.txt files, and agent/{tasks.jsonl,summary.md}.
//   2. Claude Code sessions — ~/.claude/sessions/<pid>.json registry entries,
//      plus the session transcript under ~/.claude/projects/<munged-cwd>/,
//      scanned for the /goal slash-command record and recent activity.
// ---------------------------------------------------------------------------

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Serialize, Clone)]
pub struct StageProgress {
    pub current: u32,
    pub total: u32,
    pub title: Option<String>,
    pub step: Option<u32>,
    pub step_total: Option<u32>,
}

#[derive(Serialize, Clone)]
pub struct TaskOut {
    pub title: String,
    pub status: String,
}

#[derive(Serialize, Clone)]
pub struct StageDetail {
    pub n: u32,
    pub title: Option<String>,
    pub status: String, // "done" | "current" | "pending"
    pub tasks: Vec<TaskOut>,
}

#[derive(Serialize, Clone)]
pub struct IterationStat {
    pub n: u64,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub context_pct: Option<f64>,
    pub output_tokens: Option<u64>,
}

#[derive(Serialize, Clone)]
pub struct EventOut {
    pub ts: Option<String>,
    pub iteration: Option<u64>,
    pub topic: String,
    pub text: String,
}

#[derive(Serialize, Clone)]
pub struct RunOut {
    pub started: Option<String>,
    pub ended: Option<String>,
    pub reason: Option<String>,
    pub stage: Option<u32>,
    pub prompt_head: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GoalOut {
    pub text: String,
    pub set_at: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct SessionOut {
    pub id: String,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub entrypoint: Option<String>,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MsgPreview {
    pub ts: Option<String>,
    pub role: String,
    pub text: String,
}

#[derive(Serialize, Clone)]
pub struct LoopInfo {
    pub id: String,
    pub kind: String, // "ralph" | "goal" | "session"
    pub name: String,
    pub dir: String,
    pub running: bool,
    pub state: String,
    pub pid: Option<u32>,
    pub started: Option<String>,
    pub updated: Option<String>,
    pub stage: Option<StageProgress>,
    pub iteration: Option<u64>,
    pub cost_usd: Option<f64>,
    pub stages: Vec<StageDetail>,
    pub iterations: Vec<IterationStat>,
    pub events: Vec<EventOut>,
    pub history: Vec<RunOut>,
    pub summary_md: Option<String>,
    pub terminate_reason: Option<String>,
    pub goal: Option<GoalOut>,
    pub session: Option<SessionOut>,
    pub recent: Vec<MsgPreview>,
    pub last_event: Option<EventOut>,
}

// ---------------------------------------------------------------------------
// Transcript scan state (incremental — the dashboard re-scans only appended
// bytes on each poll; the CLI scans once from the top)
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct ScanState {
    pub offset: u64,
    pub goal: Option<GoalOut>,
    pub last_ts: Option<String>,
    pub recent: VecDeque<MsgPreview>,
}

pub type ScanCache = HashMap<PathBuf, ScanState>;

/// Persisted scan offsets, so short-lived invocations (CLI, menu bar plugin)
/// only read what transcripts appended since the previous run.
fn scan_cache_path() -> Option<PathBuf> {
    crate::claude_config_dir().map(|d| d.join("loops-scan-cache.json"))
}

pub fn load_scan_cache() -> ScanCache {
    let mut cache: ScanCache = scan_cache_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    cache.retain(|p, _| p.is_file()); // drop entries for deleted transcripts
    cache
}

pub fn save_scan_cache(cache: &ScanCache) {
    let Some(path) = scan_cache_path() else {
        return;
    };
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = fs::write(path, json);
    }
}

// ---------------------------------------------------------------------------
// Collection entry point
// ---------------------------------------------------------------------------

pub fn collect_loops(extra_roots: &[PathBuf], cache: &mut ScanCache) -> Vec<LoopInfo> {
    let mut ralph_dirs = find_ralph_dirs(&scan_roots(extra_roots));
    for d in ralph_dirs_from_ps() {
        if !ralph_dirs.contains(&d) {
            ralph_dirs.push(d);
        }
    }

    let mut loops: Vec<LoopInfo> = ralph_dirs.iter().filter_map(|d| parse_ralph(d)).collect();

    // Sessions driven by a discovered ralph loop (same cwd, sdk entrypoint)
    // are the loop's backend, not a separate loop.
    let ralph_cwds: Vec<String> = loops.iter().map(|l| l.dir.clone()).collect();
    for s in parse_sessions(cache) {
        let is_ralph_backend = s
            .session
            .as_ref()
            .is_some_and(|x| x.entrypoint.as_deref() == Some("sdk-cli"))
            && ralph_cwds.contains(&s.dir);
        if !is_ralph_backend {
            loops.push(s);
        }
    }

    loops.sort_by(|a, b| {
        (
            b.running,
            b.kind == "ralph" || b.kind == "goal",
            b.updated.clone(),
        )
            .cmp(&(
                a.running,
                a.kind == "ralph" || a.kind == "goal",
                a.updated.clone(),
            ))
    });
    loops
}

fn scan_roots(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![];
    if let Ok(v) = std::env::var("CLAUDE_USAGE_LOOP_ROOTS") {
        roots.extend(v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    roots.extend(extra.iter().cloned());
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(home) = std::env::var("HOME") {
        let repos = PathBuf::from(&home).join("Repos");
        if repos.is_dir() {
            roots.push(repos);
        }
    }
    let mut seen = vec![];
    for r in roots {
        let c = r.canonicalize().unwrap_or(r);
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Ralph discovery
// ---------------------------------------------------------------------------

fn find_ralph_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    for root in roots {
        walk_for_ralph(root, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk_for_ralph(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if dir.join(".ralph").is_dir() {
        if let Ok(c) = dir.canonicalize() {
            out.push(c);
        }
        return; // nested loops under a loop project aren't a thing
    }
    if depth >= 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(std::result::Result::ok) {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            walk_for_ralph(&p, depth + 1, out);
        }
    }
}

/// Running `ralph run … -P <proj>/.ralph/<prompt>` processes reveal loop dirs
/// outside the scanned roots.
fn ralph_dirs_from_ps() -> Vec<PathBuf> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "command="])
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut dirs = vec![];
    for line in text.lines() {
        if !line.contains("ralph run") {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = toks.iter().position(|t| *t == "-P") {
            if let Some(prompt) = toks.get(i + 1) {
                // <proj>/.ralph/stageN-prompt.txt → <proj>
                let mut p = PathBuf::from(prompt);
                while p.pop() {
                    if p.file_name().is_some_and(|n| n == ".ralph") {
                        p.pop();
                        if let Ok(c) = p.canonicalize() {
                            dirs.push(c);
                        }
                        break;
                    }
                }
            }
        }
    }
    dirs
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

// ---------------------------------------------------------------------------
// Ralph parsing
// ---------------------------------------------------------------------------

fn parse_ralph(proj: &Path) -> Option<LoopInfo> {
    let ralph = proj.join(".ralph");
    if !ralph.is_dir() {
        return None;
    }

    let lock: Option<Value> = fs::read_to_string(ralph.join("loop.lock"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let lock_pid = lock
        .as_ref()
        .and_then(|l| l.get("pid"))
        .and_then(serde_json::Value::as_u64)
        .map(|p| p as u32);
    let running = lock_pid.is_some_and(pid_alive);

    // Active (or last) events file
    let events_rel = fs::read_to_string(ralph.join("current-events")).ok();
    let events_path = events_rel
        .as_deref()
        .map(|r| proj.join(r.trim()))
        .filter(|p| p.is_file());
    let raw_events: Vec<Value> = events_path
        .as_deref()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default();

    let stage_titles = stage_titles(&ralph);
    let total_stages = stage_titles.keys().max().copied().unwrap_or(0);

    // Current stage: latest task.start prompt, falling back to the lock prompt
    let task_start_prompt = raw_events
        .iter()
        .rev()
        .find(|e| e.get("topic").and_then(|t| t.as_str()) == Some("task.start"))
        .and_then(|e| e.get("payload").and_then(|p| p.as_str()))
        .map(String::from)
        .or_else(|| {
            lock.as_ref()
                .and_then(|l| l.get("prompt"))
                .and_then(|p| p.as_str())
                .map(String::from)
        });
    let current_stage = task_start_prompt.as_deref().and_then(parse_stage_number);

    // Step j/k from the newest build.done that mentions one
    let (step, step_total) = raw_events
        .iter()
        .rev()
        .filter(|e| e.get("topic").and_then(|t| t.as_str()) == Some("build.done"))
        .filter_map(|e| e.get("payload").and_then(|p| p.as_str()))
        .find_map(parse_step)
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((None, None));

    // Iteration stats
    let mut iterations: Vec<IterationStat> = vec![];
    for e in &raw_events {
        if e.get("topic").and_then(|t| t.as_str()) != Some("iteration.summary") {
            continue;
        }
        let Some(stats) = stats_value(e) else {
            continue;
        };
        iterations.push(IterationStat {
            n: e.get("iteration")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            duration_ms: stats.get("duration_ms").and_then(serde_json::Value::as_u64),
            cost_usd: stats.get("cost_usd").and_then(serde_json::Value::as_f64),
            num_turns: stats.get("num_turns").and_then(serde_json::Value::as_u64),
            context_pct: stats.get("context_pct").and_then(serde_json::Value::as_f64),
            output_tokens: stats
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64),
        });
    }
    let iteration = raw_events
        .iter()
        .filter_map(|e| e.get("iteration").and_then(serde_json::Value::as_u64))
        .max();
    let cost_usd = {
        let c: f64 = iterations.iter().filter_map(|i| i.cost_usd).sum();
        (c > 0.0).then_some(c)
    };

    let terminate_reason = raw_events
        .iter()
        .rev()
        .find(|e| e.get("topic").and_then(|t| t.as_str()) == Some("loop.terminate"))
        .and_then(|e| e.get("payload").and_then(|p| p.as_str()))
        .and_then(parse_terminate_reason);

    let events: Vec<EventOut> = raw_events
        .iter()
        .rev()
        .take(100)
        .map(event_out)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let last_event = events.last().cloned();

    let history = parse_history(&ralph);
    let tasks_by_stage = parse_stage_tasks(&ralph);

    // Per-stage status: closed tasks or an earlier stage number mean done
    let mut stages: Vec<StageDetail> = vec![];
    for n in 1..=total_stages {
        let tasks = tasks_by_stage.get(&n).cloned().unwrap_or_default();
        let all_closed = !tasks.is_empty() && tasks.iter().all(|t| t.status == "closed");
        let status = match current_stage {
            Some(c) if n == c && running => "current",
            Some(c) if n < c || all_closed => "done",
            Some(c) if n == c => "current",
            _ if all_closed => "done",
            _ => "pending",
        };
        stages.push(StageDetail {
            n,
            title: stage_titles.get(&n).cloned(),
            status: status.into(),
            tasks,
        });
    }

    let started = if running {
        lock.as_ref()
            .and_then(|l| l.get("started"))
            .and_then(|s| s.as_str())
            .map(String::from)
    } else {
        raw_events
            .iter()
            .find_map(|e| e.get("ts").and_then(|t| t.as_str()).map(String::from))
    };
    let updated = raw_events
        .iter()
        .rev()
        .find_map(|e| e.get("ts").and_then(|t| t.as_str()).map(String::from));

    let state = if running {
        "running".into()
    } else {
        let reason = terminate_reason
            .clone()
            .or_else(|| history.iter().rev().find_map(|r| r.reason.clone()));
        match reason.as_deref() {
            Some(r) if r.contains("fail") => "failed".into(),
            Some(r) if r.contains("complete") || r.contains("success") => "completed".into(),
            _ => "stopped".into(),
        }
    };

    let name = proj
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| proj.display().to_string());

    Some(LoopInfo {
        id: format!("ralph:{}", proj.display()),
        kind: "ralph".into(),
        name,
        dir: proj.display().to_string(),
        running,
        state,
        pid: lock_pid.filter(|_| running),
        started,
        updated,
        stage: current_stage.map(|c| StageProgress {
            current: c,
            total: total_stages.max(c),
            title: stage_titles.get(&c).cloned(),
            step,
            step_total,
        }),
        iteration,
        cost_usd,
        stages,
        iterations,
        events,
        history,
        summary_md: fs::read_to_string(ralph.join("agent/summary.md"))
            .ok()
            .map(|s| s.chars().take(4000).collect()),
        terminate_reason,
        goal: None,
        session: None,
        recent: vec![],
        last_event,
    })
}

/// iteration.summary payloads arrive as either a JSON string or an inline
/// object; normalize both to a Value.
fn stats_value(e: &Value) -> Option<Value> {
    match e.get("payload") {
        Some(Value::String(s)) => serde_json::from_str(s).ok(),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => None,
    }
}

fn event_out(e: &Value) -> EventOut {
    let topic = e
        .get("topic")
        .and_then(|t| t.as_str())
        .unwrap_or("?")
        .to_string();
    // iteration.summary carries stats (as an object or a JSON string) —
    // compact them; everything else is prose.
    let stats = (topic == "iteration.summary")
        .then(|| stats_value(e))
        .flatten();
    let text = match (stats, e.get("payload")) {
        (Some(st), _) => {
            // ralph reports zeros for fields it can't measure (e.g. cost on a
            // subscription backend) — show only what carries signal
            let mut parts = vec![fmt_ms(
                st.get("duration_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            )];
            if let Some(t) = st
                .get("num_turns")
                .and_then(serde_json::Value::as_u64)
                .filter(|&t| t > 0)
            {
                parts.push(format!("{t} turns"));
            }
            if let Some(c) = st
                .get("cost_usd")
                .and_then(serde_json::Value::as_f64)
                .filter(|&c| c > 0.0)
            {
                parts.push(format!("${c:.2}"));
            }
            if let Some(p) = st
                .get("context_pct")
                .and_then(serde_json::Value::as_f64)
                .filter(|&p| p > 0.0)
            {
                parts.push(format!("ctx {p:.0}%"));
            }
            parts.join(" · ")
        }
        (None, Some(Value::String(s))) => truncate(s, 500),
        _ => String::new(),
    };
    EventOut {
        ts: e.get("ts").and_then(|t| t.as_str()).map(String::from),
        iteration: e.get("iteration").and_then(serde_json::Value::as_u64),
        topic,
        text,
    }
}

fn stage_titles(ralph: &Path) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(ralph) else {
        return out;
    };
    for e in entries.filter_map(std::result::Result::ok) {
        let name = e.file_name();
        let name = name.to_string_lossy().into_owned();
        let Some(n) = name
            .strip_prefix("stage")
            .and_then(|r| r.strip_suffix("-prompt.txt"))
            .and_then(|d| d.parse::<u32>().ok())
        else {
            continue;
        };
        let title = fs::read_to_string(e.path())
            .ok()
            .and_then(|s| s.lines().next().map(String::from))
            .and_then(|l| extract_title(&l));
        out.insert(n, title.unwrap_or_else(|| format!("stage {n}")));
    }
    out
}

/// "Implement STAGE 14 ONLY of .ralph/specs/… — tour-founder-bar." → "tour-founder-bar"
fn extract_title(line: &str) -> Option<String> {
    let after = line.rsplit_once('—').map(|(_, t)| t)?;
    let t = after.trim().trim_end_matches('.').trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn parse_stage_number(text: &str) -> Option<u32> {
    let upper = text.to_uppercase();
    let i = upper.find("STAGE")?;
    let rest = &upper[i + 5..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// "… stage 14 step 3/6: …" → (3, 6)
fn parse_step(text: &str) -> Option<(u32, u32)> {
    let i = text.find("step ")?;
    let rest = &text[i + 5..];
    let (a, rest) = rest.split_once('/')?;
    let b: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((a.trim().parse().ok()?, b.parse().ok()?))
}

fn parse_terminate_reason(payload: &str) -> Option<String> {
    let i = payload.find("## Reason")?;
    payload[i + 9..]
        .split_whitespace()
        .next()
        .map(|s| s.trim_start_matches('#').to_string())
        .filter(|s| !s.is_empty())
}

fn parse_history(ralph: &Path) -> Vec<RunOut> {
    let Ok(text) = fs::read_to_string(ralph.join("history.jsonl")) else {
        return vec![];
    };
    let mut runs: Vec<RunOut> = vec![];
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = v.get("ts").and_then(|t| t.as_str()).map(String::from);
        let t = v.get("type").cloned().unwrap_or_default();
        match t.get("kind").and_then(|k| k.as_str()) {
            Some("loop_started") => {
                let prompt = t.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
                let head = truncate(prompt.lines().next().unwrap_or(""), 160);
                runs.push(RunOut {
                    started: ts,
                    ended: None,
                    reason: None,
                    stage: parse_stage_number(prompt),
                    prompt_head: head,
                });
            }
            Some("loop_completed") => {
                if let Some(last) = runs.last_mut().filter(|r| r.ended.is_none()) {
                    last.ended = ts;
                    last.reason = t.get("reason").and_then(|r| r.as_str()).map(String::from);
                }
            }
            _ => {}
        }
    }
    runs
}

fn parse_stage_tasks(ralph: &Path) -> HashMap<u32, Vec<TaskOut>> {
    let mut out: HashMap<u32, Vec<TaskOut>> = HashMap::new();
    let Ok(text) = fs::read_to_string(ralph.join("agent/tasks.jsonl")) else {
        return out;
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
        let Some(n) = key
            .strip_prefix("stage")
            .and_then(|r| r.split(':').next())
            .and_then(|d| d.parse::<u32>().ok())
        else {
            continue;
        };
        out.entry(n).or_default().push(TaskOut {
            title: v
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("?")
                .to_string(),
            status: v
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("open")
                .to_string(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Claude Code sessions (/goal loops)
// ---------------------------------------------------------------------------

fn parse_sessions(cache: &mut ScanCache) -> Vec<LoopInfo> {
    let Some(config_dir) = crate::claude_config_dir() else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(config_dir.join("sessions")) else {
        return vec![];
    };
    let mut out = vec![];
    for e in entries.filter_map(std::result::Result::ok) {
        let name = e.file_name();
        let name = name.to_string_lossy();
        // registry entries are <pid>.json; skip summaries etc.
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if !stem.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let pid = v
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .map(|p| p as u32);
        if !pid.is_some_and(pid_alive) {
            continue; // stale registry entry
        }
        let session_id = v
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let cwd = v
            .get("cwd")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let status = v.get("status").and_then(|s| s.as_str()).map(String::from);

        // Transcript: ~/.claude/projects/<cwd with non-alnum → '-'>/<id>.jsonl
        let munged: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let transcript = config_dir
            .join("projects")
            .join(&munged)
            .join(format!("{session_id}.jsonl"));
        let scan = cache.entry(transcript.clone()).or_default();
        scan_transcript(&transcript, scan);

        let updated = scan.last_ts.clone().or_else(|| {
            v.get("updatedAt")
                .and_then(serde_json::Value::as_i64)
                .and_then(epoch_ms_to_rfc3339)
        });
        let started = v
            .get("startedAt")
            .and_then(serde_json::Value::as_i64)
            .and_then(epoch_ms_to_rfc3339);
        let has_goal = scan.goal.is_some();

        out.push(LoopInfo {
            id: format!("session:{session_id}"),
            kind: if has_goal { "goal" } else { "session" }.into(),
            name: v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(&session_id)
                .to_string(),
            dir: cwd,
            running: true,
            state: status.clone().unwrap_or_else(|| "live".into()),
            pid,
            started,
            updated,
            stage: None,
            iteration: None,
            cost_usd: None,
            stages: vec![],
            iterations: vec![],
            events: vec![],
            history: vec![],
            summary_md: None,
            terminate_reason: None,
            goal: scan.goal.clone(),
            session: Some(SessionOut {
                id: session_id,
                status,
                kind: v.get("kind").and_then(|k| k.as_str()).map(String::from),
                entrypoint: v
                    .get("entrypoint")
                    .and_then(|k| k.as_str())
                    .map(String::from),
                version: v.get("version").and_then(|k| k.as_str()).map(String::from),
            }),
            recent: scan.recent.iter().cloned().collect(),
            last_event: None,
        });
    }
    out
}

/// Incrementally scan a transcript for the /goal command record, the last
/// activity timestamp, and a short feed of recent messages. Only complete
/// (newline-terminated) lines are consumed; a partial trailing line is left
/// for the next scan.
fn scan_transcript(path: &Path, state: &mut ScanState) {
    let Ok(mut f) = fs::File::open(path) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < state.offset {
        *state = ScanState::default(); // truncated/rotated — rescan
    }
    if len == state.offset {
        return;
    }
    if f.seek(SeekFrom::Start(state.offset)).is_err() {
        return;
    }
    let mut reader = BufReader::with_capacity(256 * 1024, f);
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 || !line.ends_with('\n') {
            break;
        }
        state.offset += n as u64;
        scan_line(&line, state);
    }
}

fn scan_line(line: &str, state: &mut ScanState) {
    if let Some(ts) = extract_json_string(line, "\"timestamp\":\"") {
        state.last_ts = Some(ts);
    }

    // A real /goal command record is a user entry whose message content BEGINS
    // with the command tag — the tag appearing mid-line is just conversation
    // text (e.g. a transcript that discusses /goal).
    if line.contains("\"type\":\"user\"")
        && (line.contains("\"content\":\"<command-name>/goal</command-name>")
            || line.contains("\"text\":\"<command-name>/goal</command-name>"))
    {
        let args = find_tag(line, "<command-args>", "</command-args>")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(text) = args {
            state.goal = Some(GoalOut {
                text: truncate(&text, 500),
                set_at: state.last_ts.clone(),
            });
        }
        return;
    }

    let role = if line.starts_with("{\"parentUuid") || line.contains("\"type\":\"user\"") {
        if line.contains("\"type\":\"user\"") {
            "user"
        } else if line.contains("\"type\":\"assistant\"") {
            "assistant"
        } else {
            return;
        }
    } else {
        return;
    };
    if line.contains("tool_result") || line.contains("tool_use_id") {
        return;
    }
    let text = extract_json_string(line, "\"text\":\"")
        .or_else(|| extract_json_string(line, "\"content\":\""));
    let Some(text) = text else { return };
    let text = text.trim();
    // any tag-opening message is harness plumbing (<system…>, <command-name>,
    // <local-command-stdout>…), not conversation worth previewing
    if text.is_empty() || text.starts_with('<') || text.starts_with("Caveat:") {
        return;
    }
    state.recent.push_back(MsgPreview {
        ts: state.last_ts.clone(),
        role: role.into(),
        text: truncate(text, 240),
    });
    while state.recent.len() > 8 {
        state.recent.pop_front();
    }
}

/// Extract and unescape the JSON string literal that follows `pat` in `line`.
fn extract_json_string(line: &str, pat: &str) -> Option<String> {
    let start = line.find(pat)? + pat.len() - 1; // include the opening quote
    let bytes = line.as_bytes();
    let mut end = start + 1;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'"' => {
                return serde_json::from_str::<String>(&line[start..=end]).ok();
            }
            _ => end += 1,
        }
    }
    None
}

/// Extract text between two literal tags, unescaping JSON escapes.
fn find_tag(line: &str, open: &str, close: &str) -> Option<String> {
    let s = line.find(open)? + open.len();
    let e = line[s..].find(close)? + s;
    let frag = &line[s..e];
    serde_json::from_str::<String>(&format!("\"{frag}\"")).ok()
}

// ---------------------------------------------------------------------------
// Dismissals and session control (used by the menu bar)
// ---------------------------------------------------------------------------

fn dismissed_path() -> Option<PathBuf> {
    crate::claude_config_dir().map(|d| d.join("loops-dismissed.json"))
}

/// loop id → RFC 3339 dismissed-at. A dismissed loop reappears automatically
/// if it becomes active again (running, or updated after the dismissal).
pub fn load_dismissed() -> HashMap<String, String> {
    dismissed_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn is_dismissed(l: &LoopInfo, dismissed: &HashMap<String, String>) -> bool {
    if l.running {
        return false;
    }
    let Some(at) = dismissed.get(&l.id).and_then(|s| parse_ts(s)) else {
        return false;
    };
    match l.updated.as_deref().and_then(parse_ts) {
        Some(updated) => updated <= at,
        None => true,
    }
}

pub fn dismiss(id_or_name: &str) -> anyhow::Result<()> {
    let mut cache = load_scan_cache();
    let all = collect_loops(&[], &mut cache);
    let target = all
        .iter()
        .find(|l| l.id == id_or_name || l.name == id_or_name)
        .ok_or_else(|| anyhow::anyhow!("no loop matches '{id_or_name}'"))?;
    let mut dismissed = load_dismissed();
    dismissed.insert(target.id.clone(), Utc::now().to_rfc3339());
    // drop stale entries whose loop no longer exists
    dismissed.retain(|id, _| all.iter().any(|l| &l.id == id));
    let path = dismissed_path().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    fs::write(&path, serde_json::to_string_pretty(&dismissed)?)?;
    println!("Dismissed {} from the menu bar.", target.name);
    Ok(())
}

/// SIGTERM a registered Claude Code session. Refuses pids that aren't in the
/// session registry or whose process isn't actually claude — a recycled pid
/// must never take down an unrelated process.
pub fn quit_session(pid: u32) -> anyhow::Result<()> {
    let config_dir = crate::claude_config_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let reg = config_dir.join("sessions").join(format!("{pid}.json"));
    if !reg.is_file() {
        return Err(anyhow::anyhow!(
            "pid {pid} is not a registered Claude session"
        ));
    }
    let cmdline = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if !cmdline.contains("claude") {
        return Err(anyhow::anyhow!(
            "pid {} is not running claude (got: {})",
            pid,
            cmdline.trim()
        ));
    }
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .output()?;
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !pid_alive(pid) {
            let _ = fs::remove_file(&reg); // tidy the registry entry it left behind
            println!("Session {pid} quit. Its transcript is saved; `claude --resume` restores it.");
            return Ok(());
        }
    }
    Err(anyhow::anyhow!(
        "sent SIGTERM but pid {pid} is still running — it may be mid-task"
    ))
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    s.parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn epoch_ms_to_rfc3339(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

pub fn ago(ts: &str, now: DateTime<Utc>) -> String {
    let Some(t) = parse_ts(ts) else {
        return "?".into();
    };
    let secs = (now - t).num_seconds().max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

// ---------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------

pub fn print_loops(loops: &[LoopInfo]) {
    let now = Utc::now();
    let ralphs: Vec<_> = loops.iter().filter(|l| l.kind == "ralph").collect();
    let sessions: Vec<_> = loops.iter().filter(|l| l.kind != "ralph").collect();

    if loops.is_empty() {
        println!("No loops found. Ralph loops are discovered by scanning for .ralph/ dirs");
        println!("(cwd, ~/Repos, --root, or CLAUDE_USAGE_LOOP_ROOTS); sessions come from");
        println!("~/.claude/sessions.");
        return;
    }

    if !ralphs.is_empty() {
        println!("RALPH LOOPS");
        for l in &ralphs {
            let dot = state_icon(&l.state, l.running);
            let stage = l.stage.as_ref().map(|s| {
                let mut txt = format!("stage {}/{}", s.current, s.total);
                if let Some(t) = &s.title {
                    txt.push_str(&format!(" — {t}"));
                }
                if let (Some(a), Some(b)) = (s.step, s.step_total) {
                    txt.push_str(&format!(" · step {a}/{b}"));
                }
                txt
            });
            let mut extras = vec![];
            if let Some(s) = stage {
                extras.push(s);
            }
            if let Some(i) = l.iteration {
                extras.push(format!("it {i}"));
            }
            if let Some(c) = l.cost_usd {
                extras.push(format!("${c:.2}"));
            }
            if !l.running {
                extras.push(l.state.clone());
                // the reason repeats the state for clean exits — only show it
                // when it adds information (e.g. consecutive_failures)
                if let Some(r) = l.terminate_reason.as_ref().filter(|r| **r != l.state) {
                    extras.push(format!("({r})"));
                }
            }
            if let Some(u) = &l.updated {
                extras.push(ago(u, now));
            }
            println!("{} {:<24} {}", dot, l.name, extras.join(" · "));
            if let Some(ev) = &l.last_event {
                println!(
                    "   last: {} — {}",
                    ev.topic,
                    truncate(&ev.text.replace('\n', " "), 110)
                );
            }
        }
        println!();
    }

    if !sessions.is_empty() {
        println!("CLAUDE SESSIONS");
        for l in &sessions {
            let dot = state_icon(&l.state, l.running);
            let mut extras = vec![l.state.clone()];
            if let Some(u) = &l.updated {
                extras.push(ago(u, now));
            }
            extras.push(l.dir.clone());
            let goal = l
                .goal
                .as_ref()
                .map(|g| format!("  [GOAL] {}", truncate(&g.text, 80)))
                .unwrap_or_default();
            println!("{} {:<24} {}{}", dot, l.name, extras.join(" · "), goal);
        }
    }
}

fn state_icon(state: &str, running: bool) -> &'static str {
    match state {
        "running" | "busy" => "🟢",
        "failed" => "🔴",
        "completed" => "✅",
        _ if running => "🟡",
        _ => "⚪",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_number_from_prompts() {
        assert_eq!(
            parse_stage_number("Implement STAGE 14 ONLY of x — y."),
            Some(14)
        );
        assert_eq!(parse_stage_number("Continue STAGE 1 of x — y."), Some(1));
        assert_eq!(parse_stage_number("Polish the showcase video."), None);
    }

    #[test]
    fn step_from_build_done() {
        assert_eq!(
            parse_step("collection stage 5 step 1/6: tables"),
            Some((1, 6))
        );
        assert_eq!(parse_step("all gates pass"), None);
    }

    #[test]
    fn title_from_prompt_head() {
        assert_eq!(
            extract_title(
                "Implement STAGE 14 ONLY of .ralph/specs/collection/ — tour-founder-bar."
            ),
            Some("tour-founder-bar".into())
        );
        assert_eq!(extract_title("no dash here"), None);
    }

    #[test]
    fn terminate_reason() {
        assert_eq!(
            parse_terminate_reason("## Reason\nconsecutive_failures\n\n## Status\nToo many."),
            Some("consecutive_failures".into())
        );
    }

    #[test]
    fn goal_detected_only_in_command_records() {
        let mut s = ScanState::default();
        // a real /goal command record (content begins with the tag)
        scan_line(
            r#"{"type":"user","timestamp":"2026-08-10T12:00:00Z","message":{"role":"user","content":"<command-name>/goal</command-name>\n<command-args>ship the tour to the founder</command-args>"}}"#,
            &mut s,
        );
        assert_eq!(
            s.goal.as_ref().map(|g| g.text.as_str()),
            Some("ship the tour to the founder")
        );

        // the tag merely mentioned mid-conversation must NOT register
        let mut s2 = ScanState::default();
        scan_line(
            r#"{"type":"user","timestamp":"2026-08-10T12:00:00Z","message":{"role":"user","content":"grep for <command-name>/goal</command-name> and <command-args>, </command-args> in files"}}"#,
            &mut s2,
        );
        assert!(s2.goal.is_none());
    }

    #[test]
    fn awake_duration_parses() {
        assert_eq!(crate::awake::parse_duration("8h").unwrap(), 8 * 3600);
        assert_eq!(crate::awake::parse_duration("2h30m").unwrap(), 9000);
        assert_eq!(crate::awake::parse_duration("45").unwrap(), 2700);
        assert!(crate::awake::parse_duration("abc").is_err());
    }
}
