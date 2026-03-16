# claude-usage

Tracks Claude promotional usage windows so you know when you're getting 2x and when you're not.

Anthropic is running a promo currently (at the time of writing this tool) where you have access to "doubled usage outside peak hours." This tool watches the clock and bolts onto your shell prompt, tmux, and Claude Code status bar.

**Config-driven.** New promotions go in `~/.claude/usage-windows.json`. No code changes needed.

### What it looks like

**Claude Code status bar:**
```
⚡ 2x OFF-PEAK  ends in 7h 59m
Opus 4.6 (1M context) │ █░░░░░░░ 12% │ sess 45.6k │ day 123k │ wk 890k │ ~$19.09
```

**Status check:**
```
$ claude-usage
🟢 Off-peak (2x) (2x usage)
   Ends in:      7h 55m
   Promo: March 2026 2x Promo (ends in 12d 1h)
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

Writes a default `usage-windows.json` and registers the statusline in Claude Code's `settings.json`.

**Multiple Claude instances?** I run a few locally, so `init` targets `$CLAUDE_CONFIG_DIR` if set, otherwise `~/.claude/`:

```sh
CLAUDE_CONFIG_DIR=~/.claude-home claude-usage init
CLAUDE_CONFIG_DIR=~/.claude-work claude-usage init
```

## Commands

```sh
claude-usage              # human-readable status
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
