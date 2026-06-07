# wmux

A tmux-like terminal multiplexer for the **Windows host** — sessions, windows,
and panes with tmux-style keybindings, attachable as **plain text** over SSH or
Microsoft tunnel from inside Windows Terminal, conhost, or any VT terminal.

## Why wmux

WezTerm already offers persistence and panes on Windows, but it reattaches only
through its own GPU GUI. wmux exists for the one thing that needs: **pure-text,
in-terminal attach**. SSH into a Windows host, run `wmux attach`, and you get
your running session back as text — exactly like `tmux attach` on Linux. The
session (and its processes) survives a dropped connection.

wmux is shell-agnostic: PowerShell 5.x, cmd, pwsh, or anything else, because the
daemon spawns shells under ConPTY, which translates legacy console output to VT.

## Architecture

- **`wmuxd`** — a background daemon that owns the pseudo-terminals and the
  session/window/pane tree. It outlives clients, which is what gives
  persistence. Terminal emulation is server-side: the daemon parses each pane's
  output into a cell grid and re-renders damage-tracked VT to each client.
- **`wmux`** — a thin client. It puts your terminal in raw mode and shuttles
  bytes to/from the daemon over a local named pipe (Windows) or Unix socket
  (Linux dev). Any VT terminal can render it; no GPU, no GUI.

The client auto-starts the daemon on first use.

## Usage

```
wmux new [-s <session>] [--shell <profile>]   # create a session and attach
wmux attach [-t <session>]                     # attach to an existing session
wmux ls                                         # list sessions
wmux kill-session -t <session>
wmux kill-server
wmux split-window [-h]                           # split active window (-h = left/right)
wmux new-window
wmux send-keys "<text>"                          # script input into the active pane
wmux source-file <path>                          # reload config live
```

## Default keybindings

Prefix is **`Ctrl-b`** (rebindable). After the prefix:

| Key | Action |
|-----|--------|
| `\|` | split left/right |
| `-` or `"` | split top/bottom |
| `c` | new window |
| `n` / `p` | next / previous window |
| `0`–`9` | select window by index |
| `x` | kill the active pane |
| `d` | detach (session keeps running) |
| `[` | enter copy-mode |
| `Ctrl-b` | send a literal `Ctrl-b` to the shell |

**Copy-mode:** arrows / PageUp / PageDown / Home / End or vi keys (`hjkl`) to
move; `Space` or `v` starts a selection; `Enter` or `y` yanks (and copies to
your local terminal's clipboard via OSC-52); `q` or `Escape` exits.

## Configuration

TOML at `%APPDATA%\wmux\config.toml` (Windows) or
`$XDG_CONFIG_HOME/wmux/config.toml` (Linux); override with `$WMUX_CONFIG`.

```toml
prefix = "C-a"            # change the prefix to Ctrl-a
scrollback = 5000         # lines of history per pane
default_shell = "ps5"

[[shells]]
name = "ps5"
argv = ["powershell.exe", "-NoLogo"]

[[shells]]
name = "cmd"
argv = ["cmd.exe"]

[[shells]]
name = "pwsh"
argv = ["pwsh.exe"]

[bindings]
"C-s" = "split-vertical"   # add a custom binding after the prefix
```

Reload without restarting: `wmux source-file <path>`.

## Building

Requires Rust (stable). Windows target: `x86_64-pc-windows-msvc`.

```
cargo build --release                                  # native
cargo build --release --target x86_64-pc-windows-msvc  # Windows (from any host)
```

Produces `wmux` (client) and `wmuxd` (daemon).

## Platform support

- **Windows 10 1809+** (ConPTY required). Production target.
- **Linux** — used for development and CI; the daemon runs end-to-end over Unix
  sockets so the platform-independent logic is testable without Windows.

## Status

v1. Requires Win10 1809+. Sessions survive client/connection loss but not a full
OS logout or reboot (a Windows Service for cross-logout survival is a future
idea).
