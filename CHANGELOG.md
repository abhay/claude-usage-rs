# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0, so minor
bumps may carry behavior changes).

## [Unreleased]

## [0.2.0] - 2026-08-12

The largest release yet — `claude-usage` grows from a status-bar / rate-limit
tracker into a full agent-loop dashboard and native macOS companion.

### Added

- **`loops`** — discover and visualize long-running agent loops:
  - Ralph orchestrator loops (`.ralph/` dirs under cwd, `~/Repos`, `--root`, or
    `CLAUDE_USAGE_LOOP_ROOTS`, plus any live `ralph run` process) and Claude Code
    `/goal` sessions (the `~/.claude/sessions` registry + incremental transcript
    scans for the `/goal` command record and recent activity).
  - A terminal summary, `--json`, and **`--serve`** — a local web dashboard on
    `127.0.0.1:4711` with hoverable stage meters and click-through detail
    (per-stage task checklists, per-iteration duration/cost, the live event feed,
    and run history with failure reasons).
  - `loops dismiss <name>` to hide a stopped loop, and `loops quit <pid>` — a
    guarded SIGTERM that only targets verified, registered Claude sessions.
  - Transcript scans are incremental (persisted byte offsets), so polling stays
    cheap even against multi-GB session transcripts.
- **`awake`** — Amphetamine-style keep-awake via `caffeinate` (macOS) or
  `systemd-inhibit` (Linux), with `--for <8h|90m|2h30m>` durations and a `--lid`
  mode (`sudo pmset -a disablesleep 1`) that survives a closed lid on battery.
- **`menubar`** — a native macOS menu-bar app (`menubar --install`) with
  Codex / Claude / Work tabs, plus the original SwiftBar/xbar plugin as a
  fallback (`menubar --install-swiftbar`).
- A statusline snapshot cache (`~/.claude/statusline-cache.json`) that feeds the
  menu bar between Claude Code turns, with per-session cost tracking.
- Statusline pacing markers and a burn-rate (`$/h`) figure on the usage line.

### Changed

- The promotional-window badge now appears only when a **real** favorable promo
  (multiplier > 1×) is live. The permanent peak-hours window no longer prints a
  `⚡1x` / `·1x` badge on the statusline, `label`, or `tmux`; during peak hours a
  quiet `peak · off-peak <time>` marker rides on line 2 instead, and normal
  off-peak shows nothing.
- `wait` and `watch` now key off a real bonus rather than any favorable window
  (which the permanent peak-hours window kept true almost all the time).
- The statusline cache is written atomically (temp file + rename), so concurrent
  sessions can't truncate-read it and wipe each other's cost totals.
- README overhauled with a regenerated hero plus dashboard and menu-bar mockups.

### Fixed

- The reduced peak multiplier (`0.5×`) printed as a misleading `0x`; it now
  shows `0.5x`.
- Dashboard HTTP server hardening: per-connection read/write timeouts, a
  request-body size cap, mutex-poison recovery, and `400` responses on malformed
  input.

## [0.1.x] and earlier

See the [GitHub releases](https://github.com/abhay/claude-usage-rs/releases) for
0.1.0 – 0.1.6.

[Unreleased]: https://github.com/abhay/claude-usage-rs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/abhay/claude-usage-rs/compare/v0.1.6...v0.2.0
