<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark.png">
    <img src="assets/wordmark-light.png" alt="lumux" width="420">
  </picture>

  <p><a href="README.md">English</a> · <strong>简体中文</strong></p>
</div>

**就像 tmux，但原生支持 Windows。** 一个**轻量、开源**的多路复用器——提供会话、窗口和窗格，配合 tmux 风格的快捷键——在 Windows、Linux 和 macOS 上**跨平台**运行。直接放入你现有的 `~/.tmux.conf`，并以**纯文本**形式通过 SSH 或 Microsoft 隧道，从 Windows Terminal、conhost 或任意 VT 终端中连接。

## 安装

一行命令——下载最新发布版二进制文件，校验其 checksum，然后安装（无需 root，无需构建）：

**Linux / macOS**

```sh
curl -fsSL https://hgaol.github.io/lumux/scripts/install.sh | sh
```

安装到 `~/.local/bin`（可用 `LUMUX_INSTALL_DIR` 覆盖）。

**Windows（PowerShell）**

```powershell
irm https://hgaol.github.io/lumux/scripts/install.ps1 | iex
```

把 `lumux.exe` 安装到 `%LOCALAPPDATA%\lumux\bin`，并将该目录加入用户 `PATH`。重新运行任一安装脚本即可升级。

**使用 Cargo**（任意带 Rust 工具链的平台）

```sh
cargo install lumux
```

**包管理器**

```sh
brew install hgaol/tap/lumux      # macOS / Linux（Homebrew）
scoop install lumux               # Windows（需先执行：scoop bucket add lumux https://github.com/hgaol/scoop-lumux）
winget install hgaol.lumux        # Windows
```

每个发布版的预编译二进制文件与 checksum 都在
[releases 页面](https://github.com/hgaol/lumux/releases)。若要从源码构建，见 [构建](#构建)。

## 为什么选择 lumux

WezTerm 已经能在 Windows 上提供持久化和窗格，但它只能通过自己的 GPU 图形界面重新连接。lumux 专为它做不到的那件事而生：**纯文本、终端内连接**。SSH 登录到 Windows 主机，运行 `lumux attach`，你的运行中会话就会以文本形式回到眼前——和 Linux 上的 `tmux attach` 完全一样。会话（及其进程）在连接断开后依然存活。

lumux 与 shell 无关：PowerShell 5.x、cmd、pwsh 或其他任何 shell 都可以，因为守护进程在 ConPTY 下启动 shell，由它把传统控制台输出翻译为 VT。

## 架构

lumux 是一个**单一二进制文件**，像 tmux 一样身兼两职：

- **服务端** —— 一个后台守护进程，拥有伪终端以及会话/窗口/窗格树。它的生命周期长于客户端，这正是持久化的来源。终端仿真在服务端完成：它把每个窗格的输出解析为单元格网格，并向每个客户端重新渲染带损伤跟踪的 VT。通过用隐藏的 `--server` 标志重新执行 lumux 二进制文件来启动。
- **客户端** —— 一个轻量前端。它把你的终端置于原始（raw）模式，并通过本地命名管道（Windows）或 Unix 套接字（Linux/macOS）在服务端之间来回传送字节。任意 VT 终端都能渲染它；无需 GPU，无需图形界面。

客户端在首次使用时（`lumux new` / `lumux attach`）会自动启动服务端，所以你始终只需运行 `lumux`。

## 用法

```
lumux new [-s <session>] [--shell <profile>]   # 创建会话并连接
lumux attach [-t <session>]                     # 连接到已有会话
lumux ls                                         # 列出会话
lumux kill-session -t <session>
lumux kill-server
lumux split-window [-h]                           # 分割当前窗口（-h = 左/右）
lumux new-window
lumux kill-window                                 # 关闭当前窗口
lumux send-keys "<text>"                          # 向当前窗格脚本化输入
lumux rename-window <name>                        # 重命名当前窗口
lumux rename-session <name>                       # 重命名当前会话
lumux source-file <path>                          # 实时重载配置
```

## 默认快捷键

前缀键是 **`Ctrl-b`**（可重新绑定）。按下前缀键之后：

| 按键 | 动作 |
|-----|--------|
| `"` | 上下分割 |
| `%` | 左右分割 |
| `c` | 新建窗口 |
| `n` / `p` | 下一个 / 上一个窗口 |
| `l` | 上一个（最近活动的）窗口 |
| `f` | 按名字查找并切换到窗口 |
| `0`–`9` | 按编号选择窗口 |
| `<` / `>` | 在列表中把当前窗口左移 / 右移 |
| `&` | 关闭当前窗口 |
| 方向键 | 向该方向选择窗格 |
| `;` | 上一个（最近聚焦的）窗格 |
| `Ctrl`+方向键 / `Alt`+方向键 | 调整当前窗格大小 |
| `z` | 缩放 / 取消缩放当前窗格 |
| `!` | 把当前窗格拆分到新窗口 |
| `{` / `}` | 与上一个 / 下一个窗格交换 |
| `m` | 标记 / 取消标记当前窗格（用于跨窗口 join/swap） |
| `Ctrl-o` | 旋转当前窗口中的窗格 |
| `t` | 显示时钟（按任意键关闭） |
| `S` | 切换 synchronize-panes（同时向所有窗格输入） |
| `q` | 显示窗格编号，然后按其一聚焦该窗格 |
| `Space` | 循环切换预设布局（even-horizontal/vertical、main、tiled） |
| `,` | 重命名当前窗口 |
| `$` | 重命名当前会话 |
| `:` | 命令提示符（split-window、join-pane、find-window 等） |
| `x` | 关闭当前窗格 |
| `d` | 分离（会话继续运行） |
| `[` | 进入复制模式 |
| `]` | 粘贴最近的复制缓冲区 |
| `=` | 选择一个粘贴缓冲区 |
| `s` | choose-tree：选择会话（→/← 展开/折叠到窗口） |
| `?` | 显示快捷键帮助 |
| `Ctrl-b` | 向 shell 发送一个字面量 `Ctrl-b` |

这些默认值与 tmux 一致。lumux 默认不绑定 `|`/`-`（分割）或 `r`（重载）——tmux 也不绑定——但你可以在配置中添加它们（或直接放入一份带有这些绑定的 `~/.tmux.conf`，见下文）。

**无需前缀键**（可配置的根绑定）：例如 `Alt+方向键` 可即时在窗格间跳转。当 `mouse = true` 时，点击可选择窗格，滚轮可滚入复制模式历史，拖动分隔线可调整窗格大小。

**复制模式：** 用方向键 / PageUp / PageDown / Home / End 或 vi 键（`hjkl`）移动；`w` / `b` / `e` 按词跳转，`0` / `^` / `$` 跳到行首 / 首个非空白 / 行尾，`g` / `G` 跳到滚动缓冲的顶部 / 底部；`u` / `d` 半页滚动；`/` 向前搜索、`?` 向后搜索，`n` / `N` 跳到下一个 / 上一个匹配；`Space` 或 `v` 开始选择，`Ctrl-v` / `R` 切换矩形（块）选择；`Enter` 或 `y` 复制（通过 OSC-52 复制到本地终端剪贴板，**并**压入粘贴缓冲区栈）；`q` 或 `Escape` 退出。用 `prefix ]` 粘贴最近的缓冲区，或用 `prefix =` 选择较早的一个。设置 `copy-command`（见下文）可让每次复制同时管道给一个 shell 命令。

## 命令提示符

按 **`prefix :`** 输入命令；这些命令同样可以在配置中绑定到按键（见下文）。lumux 实现的命令：

| 命令 | 说明 |
|------|------|
| `split-window [-h]` | 分割当前窗格（`-h` = 左/右） |
| `new-window` / `kill-window` | |
| `next-window` / `previous-window` / `last-window` | |
| `select-window -t N` | 切换到窗口 N |
| `last-pane` | 跳到上一个聚焦的窗格 |
| `kill-pane [-t .N]` | 关闭当前窗格，或窗格 N |
| `swap-pane [-U\|-D] [-t .N]` | 与上/下一个窗格交换，或与窗格 N 交换 |
| `break-pane` | 把当前窗格移入独立窗口 |
| `join-pane [-h\|-v] [-s N]` | 把窗口 N 的窗格（或已标记的窗格）拉入当前窗口 |
| `rotate-window [-U]` | 旋转当前窗口中的窗格 |
| `swap-window [-s A] -t B` | 按序号交换两个窗口 |
| `move-window -t N` | 把当前窗口移到序号 N |
| `select-layout [NAME]` | 应用预设布局（`even-horizontal`、`even-vertical`、`main-vertical`、`main-horizontal`、`tiled`）；不带名字则循环 |
| `next-layout` / `previous-layout` | 循环到下一个 / 上一个预设布局 |
| `select-pane -L\|-R\|-U\|-D` / `select-pane -t .N` | 把焦点移到相邻窗格，或移到窗格 N |
| `resize-pane -L\|-R\|-U\|-D [N]` | 把当前窗格按 N 个字符调整大小（不带 N 则用交互式的微调步长） |
| `resize-pane -Z` | 切换当前窗格的缩放 |
| `copy-mode` / `clock-mode` | 进入复制模式 / 显示时钟覆盖层 |
| `rename-window <name>` / `rename-session <name>` | |
| `find-window <query>` | 切换到第一个名字匹配的窗口 |
| `synchronize-panes [on\|off]` | 同时向所有窗格输入 |
| `display-panes` | 显示窗格编号 |
| `display-message <text>` | 在状态栏闪现一条消息 |
| `send-keys [-l] <keys>` | 向当前窗格发送按键（`Enter` / `C-c` 等键名，或用 `-l` 发送字面文本） |
| `set-buffer [-b name] <text>` | 把文本存入粘贴缓冲区 |
| `paste-buffer [-b name]` | 粘贴缓冲区（指定名字或最近的） |
| `save-buffer [-b name] <path>` / `load-buffer <path>` | 缓冲区文件读写 |
| `delete-buffer -b name` | 删除命名缓冲区 |
| `capture-pane` | 把可见窗格文本复制到粘贴缓冲区 |
| `respawn-pane` | 在已死窗格中重启 shell |
| `run-shell <cmd>` | 运行 shell 命令；其输出进入粘贴缓冲区 |
| `new-session [-s NAME] [-d]` | 创建一个会话，除非带 `-d` 否则切换过去 |
| `kill-session [-t NAME]` | 关闭一个会话（不指定则关闭当前会话） |
| `kill-server` | 关闭所有会话并断开所有客户端 |
| `switch-client -t NAME` | 按名字把当前客户端切换到另一个会话 |
| `set [-g] OPTION VALUE` | 运行时修改配置项（`set mouse on`、`set base-index 1`、`set status-bg red` 等） |
| `save-state` | 立即把会话快照写入磁盘 |
| `detach-client` | 分离 |

**链式命令：** 用 `;` 连接多条命令按顺序执行——`split-window -h ; select-layout tiled`。

**目标（`-t`）：** `kill-pane` 和 `swap-pane` 接受目标——`-t N` / `-t :N` 指窗口（作用于其活动窗格），`-t .N` 指当前窗口中按序号的窗格。序号遵循 `base-index`。

**标记窗格：** `prefix m` 标记当前窗格；随后 `join-pane` 或 `swap-pane` 在没有显式来源时作用于被标记的窗格，而它可以位于另一个窗口——在一个窗口标记窗格，再把它拉入或交换到另一个窗口。

## 会话与 agent 侧边栏

一个常驻的左侧边栏（herdr 风格）：上半部分列出你的**会话**，下半部分列出每个正在运行 **agent** 的窗格及其实时状态——空闲（`○`）、工作中（`●`）、被阻塞/等待输入（红色 `●`）、已完成（`✓`）。点击会话行即可切换过去，点击 agent 行则直接跳到该 agent 的具体窗格。prefix-`s` 选择器的每个窗口行也显示相同状态。

运行时用 `:set sidebar on` / `off` 切换（会话级——它会重排该会话的所有客户端），或在配置中设默认值：

```
set -g sidebar on
set -g sidebar-width 26
```

agent 自行上报状态——无需抓屏。用 `lumux integration <agent>` 一次性接入其 hooks：

```
lumux integration claude
lumux integration codex
lumux integration copilot
```

也支持 CLI 风格的别名：`claude-code`、`codex-cli`、`openai-codex`、
`copilot-cli`、`github-copilot` 和 `github-copilot-cli`。安装器会保留无关配置，
并把受 lumux 管理的 wrapper 放入对应的 agent 配置目录；可分别用
`CLAUDE_CONFIG_DIR`、`CODEX_HOME` 或 `COPILOT_HOME` 覆盖默认目录。Copilot
使用文档规定的独立 `hooks/lumux-agent-state.json`，不会修改 `settings.json`。
请使用 GitHub Copilot CLI 1.0.22 或更高版本，以确保每个会话只触发一次可靠的
生命周期事件。

Claude Code 和 GitHub Copilot CLI 提供会话结束 hook，因此 CLI 退出后会从
侧边栏移除。Codex 提供轮次结束但没有文档支持的会话结束 hook；其适配器会在
SessionStart 时启动一个脱离终端、校验进程身份的 watcher，并在原生 Codex
进程退出时只移除对应的生命周期。Codex 会要求审核新安装的用户 hooks；请在
Codex 中打开 `/hooks` 并信任 lumux 条目。

任何程序也可以在窗格内直接上报：

```
lumux report-state working   # idle | working | blocked | done
```

## 配置

lumux 会从以下路径中第一个存在的文件读取配置（用 `$LUMUX_CONFIG` 可覆盖整个查找过程）：

| 路径 | 格式 |
|------|--------|
| `%APPDATA%\lumux\lumux.conf`（Windows）/ `$XDG_CONFIG_HOME/lumux/lumux.conf` | **tmux 语法** |
| `%APPDATA%\lumux\config.toml` / `$XDG_CONFIG_HOME/lumux/config.toml` | TOML（原生） |
| `~/.lumux.conf` | **tmux 语法** |

格式由扩展名决定：`.toml` 按 TOML 解析，其他一律按 tmux 配置语法解析。

### 带上你的 `~/.tmux.conf`

你可以把现有的 tmux 配置直接作为 `~/.lumux.conf`（或 `%APPDATA%\lumux\lumux.conf`）放进去即可使用——无需任何转换：

```sh
cp ~/.tmux.conf ~/.lumux.conf
```

lumux 会读取它支持的指令（`prefix`、`mouse`、`history-limit`、`base-index`、`default-shell`/`default-command`、`status-justify`、`status-left`/`-right`、`status-style`，以及它已实现动作的 `bind` / `bind -n`），并**对其他一切发出警告后忽略**，因此一份完整的真实 tmux.conf 也能干净加载。一个现成示例见 [`examples/lumux.conf`](examples/lumux.conf)。

`bind` 运行与命令提示符相同的命令，携带真实参数并支持 `\;` 链式——例如 `bind X new-window \; split-window -h` 会执行两条命令，`bind M-l select-layout tiled` 会应用该确切布局：

```
bind | split-window -h              # 绑定会携带 -h 参数
bind r source-file ~/.lumux.conf \; display "reloaded"
```

### 原生 TOML

TOML 格式暴露相同的设置；一份完整的 tmux 对等示例见 [`examples/config.toml`](examples/config.toml)。在 TOML 中，所有顶层键都必须出现在任何 `[table]` 表头**之前**——请把标量设置放在 `[bindings]` / `[root_bindings]` / `[[shells]]` 各节之上。

### 默认 shell

设置新会话/窗口启动哪个 shell —— 用 tmux 语法：

```
set -g default-shell powershell.exe
# 或带参数：
set -g default-command "powershell.exe -NoLogo"
```

或者在 TOML 中通过命名配置（`default_shell` + `[[shells]]`，见示例）。在 Windows 上，若未配置任何内容，lumux 默认使用 PowerShell。

### 复制到系统剪贴板工具

复制（yank）已通过 OSC-52 送达本地终端剪贴板。若还想把每次复制模式的复制*同时*管道给一个 shell 命令（tmux 的 copy-pipe），设置 `copy-command`：

```
set -s copy-command "xclip -selection clipboard -in"   # 或：pbcopy、wl-copy
```

每次复制（键盘或鼠标拖动）都会把选中文本喂给该命令的 stdin。仅限 Unix。

```toml
prefix = "C-a"            # 把前缀键改为 Ctrl-a
scrollback = 5000         # 每个窗格的历史行数
mouse = true              # 点击/滚动/拖动
base_index = 1            # 窗口/窗格从 1 开始编号
default_shell = "ps5"

# 带样式的状态栏（tmux 格式标记：#S #W #H、%H:%M、#[fg=,bg=,bold]）
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
"C-s" = "split-vertical"   # 在前缀键之后添加自定义绑定
"r" = "reload-config"

[root_bindings]            # 无需前缀键即可触发（tmux bind -n）
"M-Left" = "select-pane-left"
"M-Right" = "select-pane-right"
"M-Up" = "select-pane-up"
"M-Down" = "select-pane-down"
```

无需重启即可重载：`lumux source-file <path>`（或在配置中把某个键绑定到 `reload-config`）。

## 构建

若你想从源码构建，而不使用[预编译发布版](#安装)，需要 Rust（stable）。Windows 目标：`x86_64-pc-windows-msvc`。

```
cargo build --release                                  # 本机
cargo build --release --target x86_64-pc-windows-msvc  # Windows（可从任意主机交叉编译）
```

产出单一的 `lumux` 二进制文件（它同时作为客户端和服务端运行）。

## 平台支持

- **Windows 10 1809+**（需要 ConPTY）。生产目标。
- **Linux** —— 用于开发和 CI；守护进程通过 Unix 套接字端到端运行，因此与平台无关的逻辑无需 Windows 即可测试。

## 状态

v1。需要 Win10 1809+。会话在客户端/连接丢失后存活，但无法在完整的操作系统注销或重启后存活（跨注销存活的 Windows 服务是未来的设想）。
