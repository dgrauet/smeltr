# smeltr — Usage Guide

A mental model + recipes for using smeltr in day-to-day Metal/MLX work on Apple Silicon.

## Architecture (one diagram)

```
┌─────────────────────────────────────────────────────────────────┐
│                    YOUR PYTHON / METAL PROCESS                  │
│                                                                 │
│   ┌──────────────────────────────────────────────────────────┐  │
│   │  libmetal_hook.dylib  (injected via DYLD_INSERT_LIBRARIES)│  │
│   │  → swizzles Metal API, captures command buffer lifecycle │  │
│   │  → events pushed into an SHM ring                        │  │
│   └──────────────────────────────────────────────────────────┘  │
└──────────────────────────────┬──────────────────────────────────┘
                               │ shm ring (shared memory)
                               ▼
┌─────────────────────────────────────────────────────────────────┐
│                       smeltrd (daemon)                          │
│                                                                 │
│   • Drains the ring                                             │
│   • Writes ~/.smeltr/sessions/<id>/events.cbor.zst              │
│   • Exposes a UNIX socket ($TMPDIR/smeltr.sock)                 │
└──────────┬────────────────────────────┬─────────────────────────┘
           │ socket                     │ disk
           ▼                            ▼
   ┌───────────────┐         ┌─────────────────────┐
   │ smeltr tui    │         │ smeltr sessions ... │
   │ smeltr mcp    │         │ smeltr analyze      │
   │ (live)        │         │ (replay)            │
   └───────────────┘         └─────────────────────┘
```

Three actors:
- **Producer** — your target process, with `libmetal_hook.dylib` injected. Captures Metal command buffers and emits events.
- **smeltrd** — long-running daemon. Drains the ring, persists sessions to disk, exposes a socket.
- **Consumers** — `smeltr tui`, `smeltr mcp`, `smeltr sessions`, `smeltr analyze`. Read from the daemon (live) or from disk (replay).

## The three usage modes

### 1. One-shot — capture a single run

Best for: a quick experiment, a single benchmark, a crash repro.

```bash
smeltr record python my_inference.py
```

`smeltr record` will:
1. Spawn `smeltrd` if not already running.
2. Inject `libmetal_hook.dylib` into the target process via `DYLD_INSERT_LIBRARIES`.
3. Wait for the target to exit, then flush and close the session.

The session lands in `~/.smeltr/sessions/<timestamp>-<id>/`.

### 2. Always-on — persistent daemon via LaunchAgent

Best for: regular dogfooding, multiple back-to-back runs, leaving the TUI or MCP server attached across sessions.

```bash
# One-time install:
smeltr daemon install

# Then anytime:
smeltr record python run_A.py
smeltr record python run_B.py
```

The LaunchAgent (`~/Library/LaunchAgents/com.smeltr.daemon.plist`):
- Starts `smeltrd` at every login.
- Restarts it on crash (`KeepAlive=true`, `ThrottleInterval=5s`).
- Logs to `~/.smeltr/smeltrd.log`.

To uninstall:
```bash
smeltr daemon uninstall
```

### 3. Analyze — exploit recorded sessions

| Tool | When | What |
|---|---|---|
| `smeltr tui` | During or after a run | Live UI: event feed, timeline, queue depth, MLX memory |
| `smeltr sessions list` | After | List sessions on disk |
| `smeltr sessions show <id>` | After | One-line per event-kind summary |
| `smeltr analyze <id>` | After | Run analyzer rules → findings (queue pressure, crash correlation, etc.) |
| `smeltr breakdown [--last] [<id>]` | After | Per-module GPU time breakdown for an MLX inference session |
| `smeltr mcp` (in Claude) | After | Query sessions from a Claude conversation via MCP tools |

## Typical workflow

```
[once]      smeltr daemon install              ← persistent daemon
[per run]   smeltr record <your-cmd>           ← capture
[live]      smeltr tui                         ← watch in real time (optional)
[analysis]  Ask Claude: "list my smeltr sessions" or "compare A and B"
            → Claude calls the smeltr MCP tools
```

## Getting smeltr on your PATH

Symlink the release binaries into a PATH directory:

```bash
mkdir -p ~/.local/bin
ln -sf $(pwd)/target/release/smeltr  ~/.local/bin/smeltr
ln -sf $(pwd)/target/release/smeltrd ~/.local/bin/smeltrd
```

`~/.local/bin` is already on the PATH on most setups. The symlinks follow the build target, so `cargo build --release` automatically updates them.

## MCP integration (Claude Code / Claude Desktop)

Add to `~/.claude.json` (under `mcpServers`):

```json
"smeltr": {
  "type": "stdio",
  "command": "/Users/<you>/.local/bin/smeltr",
  "args": ["mcp"],
  "env": {}
}
```

From any Claude session, you can then ask things like:
- "List my smeltr sessions"
- "Compare sessions A and B"
- "Find correlations around the queue depth peak in session X"
- "Get the crash report for the last session"

## Files & directories

| Path | Purpose |
|---|---|
| `~/.smeltr/sessions/<id>/events.cbor.zst` | Captured event stream |
| `~/.smeltr/sessions/<id>/metadata.toml` | Session metadata (argv, start/end times, …) |
| `~/.smeltr/smeltrd.log` | Daemon logs (managed by LaunchAgent) |
| `~/.smeltr/smeltrd.pid` | Current daemon PID |
| `$TMPDIR/smeltr.sock` | UNIX socket the daemon listens on |
| `~/Library/LaunchAgents/com.smeltr.daemon.plist` | LaunchAgent definition |

## Common pitfalls

### Dev dylib override

For development against an uninstalled dylib build:

```bash
SMELTR_DYLIB=$(pwd)/metal-hook/build/libmetal_hook.dylib smeltr record python my_inference.py
```

`smeltr` ships its own copy of `libmetal_hook.dylib` embedded in the binary,
so end users never need to set `SMELTR_DYLIB`. A `cargo build --release`
rebuilds both halves together — no version skew is possible.

### Two `smeltrd` processes running

Symptom: `ps aux | grep smeltrd` shows two PIDs.

Cause: an orphan daemon (from a manual `smeltrd &`) is still running alongside the LaunchAgent-managed one.

Fix: identify the LaunchAgent-managed one (`launchctl list com.smeltr.daemon` returns its PID) and kill the other(s).

### Socket-path errors on macOS

If you see `bind: invalid argument` and your `SMELTR_SOCKET` env var is set to something like `$XDG_RUNTIME_DIR/smeltr.sock`: unset it. `XDG_RUNTIME_DIR` is a Linux convention and is empty on macOS, which collapses the path to `/smeltr.sock` (too long after expansion or invalid). Default is `$TMPDIR/smeltr.sock`, which works.

## See also

- `docs/dogfood-findings.md` — real-world findings from production validation
- `README.md` — project overview
- `docs/adr/` — architecture decision records
