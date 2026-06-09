# lumux

A tmux-like terminal multiplexer for the **Windows host** — sessions, windows,
and panes with tmux-style keybindings, attachable as **plain text** over SSH or
Microsoft tunnel from inside Windows Terminal, conhost, or any VT terminal.

## Why lumux

WezTerm already offers persistence and panes on Windows, but it reattaches only
through its own GPU GUI. lumux exists for the one thing that needs: **pure-text,
in-terminal attach**. SSH into a Windows host, run `lumux attach`, and you get
your running session back as text — exactly like `tmux attach` on Linux. The
session (and its processes) survives a dropped connection.

lumux is shell-agnostic: PowerShell 5.x, cmd, pwsh, or anything else, because the
daemon spawns shells under ConPTY, which translates legacy console output to VT.

## Architecture

lumux is a **single binary** that plays two roles, like tmux:

- **Server** — a background daemon that owns the pseudo-terminals and the
  session/window/pane tree. It outlives clients, which is what gives
  persistence. Terminal emulation is server-side: it parses each pane's output
  into a cell grid and re-renders damage-tracked VT to each client. Started by
  re-execing the lumux binary with a hidden `--server` flag.
- **Client** — a thin front-end. It puts your terminal in raw mode and shuttles
  bytes to/from the server over a local named pipe (Windows) or Unix socket
  (Linux/macOS). Any VT terminal can render it; no GPU, no GUI.

The client auto-starts the server on first use (`lumux new` / `lumux attach`), so
you only ever run `lumux`.

## Usage

```
lumux new [-s <session>] [--shell <profile>]   # create a session and attach
lumux attach [-t <session>]                     # attach to an existing session
lumux ls                                         # list sessions
lumux kill-session -t <session>
lumux kill-server
lumux split-window [-h]                           # split active window (-h = left/right)
lumux new-window
lumux send-keys "<text>"                          # script input into the active pane
lumux rename-window <name>                        # rename the active window
lumux rename-session <name>                       # rename the current session
lumux source-file <path>                          # reload config live
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
| arrows | select pane in that direction |
| `H` `J` `K` `L` | resize the active pane (left/down/up/right) |
| `z` | zoom / unzoom the active pane |
| `,` | rename the current window |
| `$` | rename the current session |
| `x` | kill the active pane |
| `d` | detach (session keeps running) |
| `[` | enter copy-mode |
| `r` | reload the config file (flashes a confirmation) |
| `Ctrl-b` | send a literal `Ctrl-b` to the shell |

**Without the prefix** (configurable root bindings): e.g. `Alt+Arrow` jumps
between panes instantly. With `mouse = true`, click selects a pane, the wheel
scrolls into copy-mode history, and dragging a divider resizes panes.

**Copy-mode:** arrows / PageUp / PageDown / Home / End or vi keys (`hjkl`) to
move; `u` / `d` half-page scroll; `Space` or `v` starts a selection; `Enter` or
`y` yanks (and copies to your local terminal's clipboard via OSC-52); `q` or
`Escape` exits.

## Configuration

TOML at `%APPDATA%\lumux\config.toml` (Windows) or
`$XDG_CONFIG_HOME/lumux/config.toml` (Linux/macOS); override with `$LUMUX_CONFIG`.
A complete tmux-parity example is in [`examples/config.toml`](examples/config.toml).

In TOML, all top-level keys must come **before** any `[table]` header — keep the
scalar settings above the `[bindings]` / `[root_bindings]` / `[[shells]]`
sections.

```toml
prefix = "C-a"            # change the prefix to Ctrl-a
scrollback = 5000         # lines of history per pane
mouse = true              # click/scroll/drag
base_index = 1            # number windows/panes from 1
default_shell = "ps5"

# Styled status bar (tmux format tokens: #S #W #H, %H:%M, #[fg=,bg=,bold])
status_justify = "centre"
status_bg = "colour24"
status_left = "#[fg=white,bg=colour124,bold] REMOTE #[bg=colour24,fg=green] Session: #S "
status_right = "#[fg=cyan]%H:%M #[fg=yellow]%d-%b-%y"

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
"r" = "reload-config"

[root_bindings]            # fire without the prefix (tmux bind -n)
"M-Left" = "select-pane-left"
"M-Right" = "select-pane-right"
"M-Up" = "select-pane-up"
"M-Down" = "select-pane-down"
```

Reload without restarting: `lumux source-file <path>` (or prefix + `r`).

## Building

Requires Rust (stable). Windows target: `x86_64-pc-windows-msvc`.

```
cargo build --release                                  # native
cargo build --release --target x86_64-pc-windows-msvc  # Windows (from any host)
```

Produces a single `lumux` binary (it runs as both client and server).

## Platform support

- **Windows 10 1809+** (ConPTY required). Production target.
- **Linux** — used for development and CI; the daemon runs end-to-end over Unix
  sockets so the platform-independent logic is testable without Windows.

## Status

v1. Requires Win10 1809+. Sessions survive client/connection loss but not a full
OS logout or reboot (a Windows Service for cross-logout survival is a future
idea).
