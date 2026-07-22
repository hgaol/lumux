<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark.png">
    <img src="assets/wordmark-light.png" alt="lumux" width="420">
  </picture>

  <p><strong>English</strong> · <a href="README.zh-CN.md">简体中文</a></p>
</div>

**Like tmux, but with native Windows support.** A **lightweight, open-source**
multiplexer — sessions, windows, and panes with tmux-style keybindings — that
runs **cross-platform** on Windows, Linux, and macOS. Drop in your existing
`~/.tmux.conf` and attach as **plain text** over SSH or Microsoft tunnel from
inside Windows Terminal, conhost, or any VT terminal.

## Install

One line — downloads the latest release binary, verifies its checksum, and
installs it (no root, no build):

**Linux / macOS**

```sh
curl -fsSL https://hgaol.github.io/lumux/scripts/install.sh | sh
```

Installs to `~/.local/bin` (override with `LUMUX_INSTALL_DIR`).

**Windows (PowerShell)**

```powershell
irm https://hgaol.github.io/lumux/scripts/install.ps1 | iex
```

Installs `lumux.exe` to `%LOCALAPPDATA%\lumux\bin` and adds it to your user
`PATH`. Re-run either installer to upgrade.

**With Cargo** (any platform with a Rust toolchain)

```sh
cargo install lumux
```

**Package managers**

```sh
brew install hgaol/tap/lumux      # macOS / Linux (Homebrew)
scoop install lumux               # Windows (after: scoop bucket add lumux https://github.com/hgaol/scoop-lumux)
winget install hgaol.lumux        # Windows
```

Prebuilt binaries and checksums for each release are on the
[releases page](https://github.com/hgaol/lumux/releases). To build from source,
see [Building](#building).

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
lumux kill-window                                 # kill the active window
lumux send-keys "<text>"                          # script input into the active pane
lumux rename-window <name>                        # rename the active window
lumux rename-session <name>                       # rename the current session
lumux source-file <path>                          # reload config live
```

## Default keybindings

Prefix is **`Ctrl-b`** (rebindable). After the prefix:

| Key | Action |
|-----|--------|
| `"` | split top/bottom |
| `%` | split left/right |
| `c` | new window |
| `n` / `p` | next / previous window |
| `l` | last (previously-active) window |
| `f` | find a window by name and switch to it |
| `0`–`9` | select window by index |
| `<` / `>` | move the active window left / right in the list |
| `&` | kill the active window |
| arrows | select pane in that direction |
| `;` | last (previously-active) pane |
| `Ctrl`+arrows / `Alt`+arrows | resize the active pane |
| `z` | zoom / unzoom the active pane |
| `!` | break the active pane into a new window |
| `{` / `}` | swap the active pane with the previous / next one |
| `m` | mark / unmark the active pane (for cross-window join/swap) |
| `Ctrl-o` | rotate the panes in the active window |
| `t` | show the clock (any key closes it) |
| `S` | toggle synchronize-panes (type into all panes at once) |
| `q` | show pane numbers, then press one to focus it |
| `Space` | cycle preset layouts (even-horizontal/vertical, main, tiled) |
| `,` | rename the current window |
| `$` | rename the current session |
| `:` | command prompt (split-window, join-pane, find-window, …) |
| `x` | kill the active pane |
| `d` | detach (session keeps running) |
| `[` | enter copy-mode |
| `]` | paste the most recent copy buffer |
| `=` | choose a paste buffer |
| `s` | choose-tree: pick a session (→/← expand/collapse into windows) |
| `?` | show the key-binding help |
| `Ctrl-b` | send a literal `Ctrl-b` to the shell |

These defaults match tmux. lumux doesn't bind `|`/`-` (splits) or `r` (reload) by
default — tmux doesn't either — but you can add them in your config (or drop in a
`~/.tmux.conf` that has them; see below).

**Without the prefix** (configurable root bindings): e.g. `Alt+Arrow` jumps
between panes instantly. With `mouse = true`, click selects a pane, the wheel
scrolls into copy-mode history, and dragging a divider resizes panes.

**Copy-mode:** arrows / PageUp / PageDown / Home / End or vi keys (`hjkl`) to
move; `w` / `b` / `e` jump by word, `0` / `^` / `$` to line start / first
non-blank / end, `g` / `G` to the top / bottom of the scrollback; `u` / `d`
half-page scroll; `/` searches forward and `?` backward, with `n` / `N` to jump
to the next / previous match; `Space` or `v` starts a selection and `Ctrl-v` /
`R` toggles a rectangular (block) selection; `Enter` or `y` yanks (copying to
your local terminal's clipboard via OSC-52 **and** pushing onto the paste-buffer
stack); `q` or `Escape` exits. Paste the most recent buffer with `prefix ]`, or
pick an older one with `prefix =`. Set `copy-command` (below) to also pipe each
yank to a shell command.

## Command prompt

Press **`prefix :`** to type a command; the same commands can be bound to keys
in your config (see below). The commands lumux implements:

| Command | Notes |
|---------|-------|
| `split-window [-h]` | split the active pane (`-h` = left/right) |
| `new-window` / `kill-window` | |
| `next-window` / `previous-window` / `last-window` | |
| `select-window -t N` | switch to window N |
| `last-pane` | jump to the previously-active pane |
| `kill-pane [-t .N]` | kill the active pane, or pane N |
| `swap-pane [-U\|-D] [-t .N]` | swap with the previous / next pane, or pane N |
| `break-pane` | move the active pane into its own window |
| `join-pane [-h\|-v] [-s N]` | pull a pane from window N (or the marked pane) into this one |
| `rotate-window [-U]` | rotate the panes in the active window |
| `swap-window [-s A] -t B` | swap two windows by index |
| `move-window -t N` | move the active window to index N |
| `select-layout [NAME]` | apply a preset (`even-horizontal`, `even-vertical`, `main-vertical`, `main-horizontal`, `tiled`); bare cycles |
| `next-layout` / `previous-layout` | cycle to the next / previous preset layout |
| `select-pane -L\|-R\|-U\|-D` / `select-pane -t .N` | move focus to a neighbor, or to pane N |
| `resize-pane -L\|-R\|-U\|-D [N]` | resize the active pane by N cells (bare = the interactive nudge amount) |
| `resize-pane -Z` | toggle zoom on the active pane |
| `copy-mode` / `clock-mode` | enter copy-mode / show the clock overlay |
| `rename-window <name>` / `rename-session <name>` | |
| `find-window <query>` | switch to the first window whose name matches |
| `synchronize-panes [on\|off]` | type into every pane at once |
| `display-panes` | show pane numbers |
| `display-message <text>` | flash a message in the status line |
| `send-keys [-l] <keys>` | send keys to the active pane (key names like `Enter` / `C-c`, or `-l` for literal text) |
| `set-buffer [-b name] <text>` | store text in a paste buffer |
| `paste-buffer [-b name]` | paste a buffer (named, or the most recent) |
| `save-buffer [-b name] <path>` / `load-buffer <path>` | buffer file I/O |
| `delete-buffer -b name` | delete a named buffer |
| `capture-pane` | copy the visible pane text into a paste buffer |
| `respawn-pane` | restart the shell in a dead pane |
| `run-shell <cmd>` | run a shell command; its output goes to a paste buffer |
| `new-session [-s NAME] [-d]` | create a session, switching to it unless `-d` |
| `kill-session [-t NAME]` | kill a session (the current one if omitted) |
| `kill-server` | kill every session and detach every client |
| `switch-client -t NAME` | switch the current client to another session by name |
| `set [-g] OPTION VALUE` | change a config option at runtime (`set mouse on`, `set base-index 1`, `set status-bg red`, …) |
| `save-state` | write the session snapshot to disk now |
| `detach-client` | detach |

**Chaining:** join commands with `;` to run them in order —
`split-window -h ; select-layout tiled`.

**Targets (`-t`):** `kill-pane` and `swap-pane` take a target — `-t N` / `-t :N`
for a window (acts on its active pane), `-t .N` for a pane by index in the active
window. Indexes honor `base-index`.

**Marking panes:** `prefix m` marks the active pane; then `join-pane` or
`swap-pane` with no explicit source acts on the marked pane, which may be in
another window — mark a pane in one window and pull or swap it into another.

## Sessions & agents sidebar

A persistent left sidebar (herdr-style) lists your **sessions** on top and, below
them, every pane running an **agent** with its live status — idle (`○`), working
(`●`), blocked/needs-input (`●`, in red), or done (`✓`). Click a session row to
switch to it, or an agent row to jump straight to that agent's pane. The
prefix-`s` chooser shows the same status on each window row.

Toggle it at runtime with `:set sidebar on` / `off` (session-global — it reflows
every client of the session), or set the defaults in config:

```
set -g sidebar on
set -g sidebar-width 26
```

Agents report their own state — no screen-scraping. Wire up each agent's hooks
once with `lumux integration <agent>`:

```
lumux integration claude
lumux integration codex
lumux integration copilot
```

CLI-style aliases are also accepted: `claude-code`, `codex-cli`,
`openai-codex`, `copilot-cli`, `github-copilot`, and `github-copilot-cli`.

The installers preserve unrelated configuration and place a managed wrapper in
the agent's config directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or
`COPILOT_HOME` overrides the default). Copilot uses its dedicated documented
`hooks/lumux-agent-state.json` file, leaving `settings.json` untouched. Use
GitHub Copilot CLI 1.0.22 or newer for reliable once-per-session lifecycle
events.

Hook files are loaded when the provider starts. After installing or updating an
integration, fully exit and restart every running Claude Code, Codex, or Copilot
CLI process; an existing process will not notice the new hooks. Codex also runs
new user hooks **zero times** until they are trusted: after restarting, open
`/hooks` inside Codex and trust every Lumux entry. The installer prints these
activation steps.

Lumux panes also inherit the exact daemon endpoint and reporter binary used by
hooks, so integrations do not depend on an interactive `PATH` or the default
socket. Panes created by an older daemon lack that runtime context. After
upgrading from an older integration build, restart the daemon and recreate the
panes when convenient (note that `lumux kill-server` terminates its sessions).

Claude Code and GitHub Copilot CLI expose session-end hooks, so their sidebar
entry is removed when the CLI exits. Codex exposes turn completion but no
documented session-end hook; its adapter launches a detached,
process-identity-checked watcher at SessionStart and removes the same lifecycle
when the native Codex process exits.

Any program can report directly from inside a pane:

```
lumux report-state working   # idle | working | blocked | done
```

## Configuration

lumux reads its config from the first of these that exists (override the whole
search with `$LUMUX_CONFIG`):

| Path | Format |
|------|--------|
| `%APPDATA%\lumux\lumux.conf` (Windows) / `$XDG_CONFIG_HOME/lumux/lumux.conf` | **tmux syntax** |
| `%APPDATA%\lumux\config.toml` / `$XDG_CONFIG_HOME/lumux/config.toml` | TOML (native) |
| `~/.lumux.conf` | **tmux syntax** |

The format is chosen by extension: `.toml` is parsed as TOML, anything else as
tmux config syntax.

### Bring your `~/.tmux.conf`

You can drop your existing tmux config in as `~/.lumux.conf` (or
`%APPDATA%\lumux\lumux.conf`) and it works — no translation needed:

```sh
cp ~/.tmux.conf ~/.lumux.conf
```

lumux reads the directives it supports (`prefix`, `mouse`, `history-limit`,
`base-index`, `default-shell`/`default-command`, `status-justify`,
`status-left`/`-right`, `status-style`, `pane-active-border-style`, and `bind` /
`bind -n` for the actions it has) and **ignores everything else with a
warning**, so a full real-world tmux.conf loads cleanly. A ready example is in
[`examples/lumux.conf`](examples/lumux.conf).

A `bind` runs the same commands as the command prompt, with their real arguments
and `\;` chains — e.g. `bind X new-window \; split-window -h` runs both, and
`bind M-l select-layout tiled` applies that exact layout:

```
bind | split-window -h              # bind carries the -h argument
bind r source-file ~/.lumux.conf \; display "reloaded"
```

### Native TOML

The TOML format exposes the same settings; a complete tmux-parity example is in
[`examples/config.toml`](examples/config.toml). In TOML, all top-level keys must
come **before** any `[table]` header — keep the scalar settings above the
`[bindings]` / `[root_bindings]` / `[[shells]]` sections.

### Default shell

Set which shell new sessions/windows spawn — in tmux syntax:

```
set -g default-shell powershell.exe
# or with arguments:
set -g default-command "powershell.exe -NoLogo"
```

or in TOML via a named profile (`default_shell` + `[[shells]]`, see the example).
On Windows, with nothing configured, lumux defaults to PowerShell.

### Copy to a system clipboard tool

Yanks already reach your local terminal's clipboard via OSC-52. To *also* pipe
each copy-mode yank to a shell command (tmux's copy-pipe), set `copy-command`:

```
set -s copy-command "xclip -selection clipboard -in"   # or: pbcopy, wl-copy
```

The selected text is fed to the command's stdin on every yank (keyboard or mouse
drag). Unix-only.

### Session persistence (survive a reboot)

Out of the box, sessions survive **detach** — closing your terminal or a dropped
SSH connection leaves the daemon (and your shells) running, and `lumux attach`
drops you back in. They do **not** survive the daemon being killed, logout, or a
reboot.

Turn on `persist` for tmux-resurrect-style on-disk persistence:

```
set -g persist on          # tmux syntax
```

```toml
persist = true             # native TOML
```

When enabled, the daemon periodically saves the session **structure** —
sessions, windows, the pane split layout, and each pane's shell + working
directory — to `<config-dir>/state.bin`, and rebuilds it when a fresh daemon
starts. Save on demand any time with the command prompt: `prefix :` then
`save-state`. Like tmux-resurrect, running programs are **not** resurrected —
only the shell is relaunched in its saved directory.

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

Reload without restarting: `lumux source-file <path>` (or bind a key to
`reload-config` in your config).

## Building

To build from source instead of using a [prebuilt release](#install), you need
Rust (stable). Windows target: `x86_64-pc-windows-msvc`.

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
