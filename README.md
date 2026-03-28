# claude-usage

![claude-usage statusline](assets/statusline.png)

Tracks Claude usage windows, rate limits, and API health. Bolts onto your shell prompt, tmux, and Claude Code status bar.

Shows promotional multipliers (2x off-peak, etc.), 5-hour and 7-day rate limit usage with reset countdowns, context window fill, daily/weekly token totals, session cost, and Anthropic API status with per-component health and incident details.

**Config-driven.** New promotions go in `~/.claude/usage-windows.json`. No code changes needed.

### What it looks like

**Claude Code status bar (low usage — compact):**
```
⚡ 2x OFF-PEAK  ends in 7h 59m
Opus 4.6 (1M context) │ ctx 6% │ d 123k │ w 890k │ ~$4.51 │ 5h 9% │ 7d 1%
```

**Claude Code status bar (high usage — bars expand):**
```
⚡ 2x OFF-PEAK  ends in 7h 59m
Opus 4.6 (1M context) │ ctx ███████░ 85% │ d 123k │ w 890k │ ~$19.09 │ 5h ███████░ 92% ↻23m │ 7d ████░░░░ 45%
```

**Status check:**
```
$ claude-usage
🟢 API: All Systems Operational (just now)
🟢 Off-peak (standard) (1x usage)
   Ends in:      1d 16h
   Promo: Peak Hours Session Limit Adjustment (ongoing)
```

**Schedule across timezones:**
```
$ claude-usage schedule
Peak Hours Session Limit Adjustment
  Peak: mon, tue, wed, thu, fri UTC 13:00 – 19:00

  City               Peak start      Peak end
  ────────────────   ──────────    ──────────
  San Francisco         6:00 AM      12:00 PM
  New York              9:00 AM       3:00 PM
  London                1:00 PM       7:00 PM
  Tokyo                10:00 PM       4:00 AM
```

**API degraded (with incident detail):**
```
$ claude-usage api-status
🟠 API: Partially Degraded Service (just now)
  ⚡ Elevated error rates on Claude API [major]
  ↳ Claude API: partial_outage
```

**API degraded (5xx detected via direct probe):**
```
$ claude-usage api-status
🟠 API overloaded (529)
```

**Defer decision:**
```
$ claude-usage defer large
✅ PROCEED: large at 2x (already in favorable window)
```

## Install

**Homebrew:**
```sh
brew tap abhay/tap
brew install claude-usage
```

**Shell installer (macOS / Linux):**
```sh
curl -fsSL https://raw.githubusercontent.com/abhay/claude-usage-rs/main/install.sh | sh
```

**From source:**
```sh
cargo install --path .
```

## Setup

```sh
claude-usage init
```

Writes `usage-windows.json`, registers the statusline, and sets up the MCP server. If you have multiple `~/.claude*` directories, `init` finds and configures all of them automatically.

To update an existing config with the latest embedded default:

```sh
claude-usage init --force
```

To target a specific instance:

```sh
CLAUDE_CONFIG_DIR=~/.claude-work claude-usage init
```

## Commands

```sh
claude-usage              # human-readable status (includes API health)
claude-usage schedule     # peak/off-peak times across timezones
claude-usage watch        # monitor status changes with desktop notifications
claude-usage api-status   # check Anthropic API status (status page + direct probe)
claude-usage label        # compact PS1/Starship token: ⚡2x
claude-usage tmux         # tmux status bar segment
claude-usage statusline   # Claude Code status bar (reads JSON from stdin)
claude-usage json         # machine-readable JSON
claude-usage windows      # list all configured windows
claude-usage defer large  # should I defer this task? (small|medium|large|xl)
claude-usage wait         # block until a favorable window opens
```

## Shell integration

**Zsh / Bash** (`.zshrc` / `.bashrc`):
```sh
PROMPT='$(claude-usage label 2>/dev/null) %n@%m %~ %# '
```

**Starship** (`starship.toml`):
```toml
[custom.claude_usage]
command = "claude-usage label"
when = true
format = "[$output]($style) "
style = "bold green"
```

**tmux** (`~/.tmux.conf`):
```
set -g status-right '#(claude-usage tmux) | %H:%M'
set -g status-interval 60
```

**Block until 2x kicks in:**
```sh
claude-usage wait && claude "refactor the auth module"
```

## MCP server (Claude Code integration)

Claude can check the usage window mid-task via MCP:

```json
{
  "mcpServers": {
    "claude-usage": {
      "command": "claude-usage",
      "args": ["mcp"]
    }
  }
}
```

Or run `claude-usage init` to register it automatically.

Available tools:
- `should_defer_task`: returns a defer/proceed recommendation for a given task size

## Adding a promotion

Edit `~/.claude/usage-windows.json` and drop in an entry:

```json
{
  "id": "anthropic-summer-2026",
  "label": "Summer 2026 Promo",
  "description": "2x usage on weekends",
  "source": "https://support.claude.com/...",
  "active_range": {
    "start": "2026-06-01T00:00:00Z",
    "end":   "2026-06-30T23:59:59Z"
  },
  "tiers": [
    {
      "id": "weekend",
      "label": "Weekend (2x)",
      "multiplier": 2.0,
      "favorable": true,
      "schedule": {
        "type": "recurring",
        "days": ["sat", "sun"],
        "utc_start": "00:00",
        "utc_end": "23:59"
      }
    },
    {
      "id": "weekday",
      "label": "Weekday (1x)",
      "multiplier": 1.0,
      "favorable": false,
      "schedule": {
        "type": "recurring",
        "days": ["mon", "tue", "wed", "thu", "fri"],
        "utc_start": "00:00",
        "utc_end": "23:59"
      }
    }
  ],
  "plans": ["pro", "max", "team"]
}
```

### Schedule types

| Type | What it does |
|------|-------------|
| `recurring` | Matches specific weekdays + a UTC time window |
| `inverse_recurring` | Matches everything *outside* a recurring window |
| `always` | Matches all times (flat multiplier for the whole promo) |

## Platform support

macOS and Linux. No Windows support yet.

## License

MIT
