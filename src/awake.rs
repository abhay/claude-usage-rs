// ---------------------------------------------------------------------------
// Keep-awake (Amphetamine equivalent)
//
// `awake on` spawns a detached sleep blocker and records its pid in
// ~/.claude/keep-awake.json; `awake off` kills it. macOS uses caffeinate
// (display, idle, disk and — on AC power — system sleep). Linux uses
// systemd-inhibit holding idle/sleep/lid inhibitors.
//
// A closed lid on battery is the one thing caffeinate cannot survive; the
// --lid flag additionally runs `sudo pmset -a disablesleep 1`, which does.
// ---------------------------------------------------------------------------

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, process::Stdio};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AwakeState {
    pub pid: u32,
    pub started: String,
    pub until: Option<String>,
    pub lid: bool,
    pub method: String,
}

fn state_path() -> Option<PathBuf> {
    crate::claude_config_dir().map(|d| d.join("keep-awake.json"))
}

/// `sudo pmset -a disablesleep {1|0}` — the closed-lid override. Returns
/// whether the command succeeded (sudo may be declined).
#[cfg(target_os = "macos")]
fn set_disablesleep(on: bool) -> bool {
    std::process::Command::new("sudo")
        .args(["pmset", "-a", "disablesleep", if on { "1" } else { "0" }])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Current state, validated against the process table. A dead blocker's
/// state file is cleaned up (lid mode is left alone — pmset outlives us).
pub fn status() -> Option<AwakeState> {
    let path = state_path()?;
    let state: AwakeState = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    if crate::loops::pid_alive(state.pid) {
        Some(state)
    } else {
        if !state.lid {
            let _ = fs::remove_file(&path);
        }
        None
    }
}

pub fn turn_on(duration_secs: Option<u64>, lid: bool) -> Result<AwakeState> {
    if let Some(existing) = status() {
        if !lid || existing.lid {
            return Ok(existing);
        }
        turn_off()?; // upgrading to lid mode — restart the blocker
    }

    let child = {
        #[cfg(target_os = "macos")]
        {
            let mut cmd = std::process::Command::new("caffeinate");
            cmd.arg("-dims");
            if let Some(secs) = duration_secs {
                cmd.args(["-t", &secs.to_string()]);
            }
            cmd
        }
        #[cfg(target_os = "linux")]
        {
            let mut cmd = std::process::Command::new("systemd-inhibit");
            cmd.args([
                "--what=idle:sleep:handle-lid-switch",
                "--who=claude-usage",
                "--why=claude-usage awake",
                "sleep",
            ]);
            cmd.arg(
                duration_secs
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "infinity".into()),
            );
            cmd
        }
    }
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|e| anyhow!("failed to start sleep blocker: {e}"))?;

    if lid {
        #[cfg(target_os = "macos")]
        {
            eprintln!("Enabling closed-lid mode via `sudo pmset -a disablesleep 1`.");
            eprintln!("⚠️  The machine will NOT sleep at all until `claude-usage awake off`.");
            eprintln!("   Watch heat if it goes in a bag.");
            if !set_disablesleep(true) {
                return Err(anyhow!("pmset disablesleep failed (sudo declined?)"));
            }
        }
    }

    let now = Utc::now();
    let state = AwakeState {
        pid: child.id(),
        started: now.to_rfc3339(),
        until: duration_secs.map(|s| (now + chrono::Duration::seconds(s as i64)).to_rfc3339()),
        lid,
        method: if cfg!(target_os = "macos") {
            "caffeinate".into()
        } else {
            "systemd-inhibit".into()
        },
    };
    let path = state_path().ok_or_else(|| anyhow!("HOME not set"))?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path, serde_json::to_string_pretty(&state)?)?;
    Ok(state)
}

/// Returns Some(warning) if lid mode could not be reverted.
pub fn turn_off() -> Result<Option<String>> {
    let Some(path) = state_path() else {
        return Ok(None);
    };
    let state: Option<AwakeState> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(state) = state else {
        return Ok(None);
    };

    let _ = std::process::Command::new("kill")
        .arg(state.pid.to_string())
        .output();

    let mut warning = None;
    if state.lid {
        #[cfg(target_os = "macos")]
        {
            if !set_disablesleep(false) {
                warning = Some(
                    "could not revert lid mode — run `sudo pmset -a disablesleep 0` manually"
                        .to_string(),
                );
            }
        }
    }
    let _ = fs::remove_file(&path);
    Ok(warning)
}

/// "8h", "90m", "2h30m", "45" (minutes) → seconds
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return Ok(s.parse::<u64>()? * 60);
    }
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num
                .parse()
                .map_err(|_| anyhow!("bad duration: {s} (use 8h, 90m, 2h30m)"))?;
            total += match c {
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => return Err(anyhow!("bad duration unit '{c}' (use h, m, s)")),
            };
            num.clear();
        }
    }
    if !num.is_empty() {
        return Err(anyhow!("bad duration: {s} (trailing number)"));
    }
    if total == 0 {
        return Err(anyhow!("duration must be > 0"));
    }
    Ok(total)
}

pub fn print_status() {
    match status() {
        Some(s) => {
            let now = Utc::now();
            let since = s
                .started
                .parse::<DateTime<Utc>>()
                .map(|t| crate::fmt_mins(((now - t).num_minutes().max(0)) as u32))
                .unwrap_or_else(|_| "?".into());
            let until = s
                .until
                .as_deref()
                .and_then(|u| u.parse::<DateTime<Utc>>().ok())
                .map(|t| {
                    format!(
                        ", {} left",
                        crate::fmt_mins(((t - now).num_minutes().max(0)) as u32)
                    )
                })
                .unwrap_or_default();
            println!(
                "☕ Awake ({}, {} elapsed{}){}",
                s.method,
                since,
                until,
                if s.lid { " — closed-lid mode ON" } else { "" }
            );
            if !s.lid {
                println!("   Lid-close on battery still sleeps. Use: claude-usage awake on --lid");
            }
        }
        None => println!("💤 Not keeping awake. Enable: claude-usage awake on [--for 8h] [--lid]"),
    }
}
