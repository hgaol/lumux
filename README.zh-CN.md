<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/wordmark.png">
    <img src="assets/wordmark-light.png" alt="lumux" width="420">
  </picture>

  <p><a href="README.md">English</a> · <strong>简体中文</strong></p>
</div>

**就像 tmux，但原生支持 Windows。** 一个**轻量、开源**的多路复用器——提供会话、窗口和窗格，配合 tmux 风格的快捷键——在 Windows、Linux 和 macOS 上**跨平台**运行。直接放入你现有的 `~/.tmux.conf`，并以**纯文本**形式通过 SSH 或 Microsoft 隧道，从 Windows Terminal、conhost 或任意 VT 终端中连接。

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
| `\|` | 左右分割 |
| `-` 或 `"` | 上下分割 |
| `c` | 新建窗口 |
| `n` / `p` | 下一个 / 上一个窗口 |
| `l` | 上一个（最近活动的）窗口 |
| `0`–`9` | 按编号选择窗口 |
| `&` | 关闭当前窗口 |
| 方向键 | 向该方向选择窗格 |
| `;` | 上一个（最近聚焦的）窗格 |
| `H` `J` `K` `L` | 调整当前窗格大小（左/下/上/右） |
| `z` | 缩放 / 取消缩放当前窗格 |
| `Space` | 循环切换预设布局（even-horizontal/vertical、main、tiled） |
| `,` | 重命名当前窗口 |
| `$` | 重命名当前会话 |
| `x` | 关闭当前窗格 |
| `d` | 分离（会话继续运行） |
| `[` | 进入复制模式 |
| `r` | 重载配置文件（闪烁确认提示） |
| `Ctrl-b` | 向 shell 发送一个字面量 `Ctrl-b` |

**无需前缀键**（可配置的根绑定）：例如 `Alt+方向键` 可即时在窗格间跳转。当 `mouse = true` 时，点击可选择窗格，滚轮可滚入复制模式历史，拖动分隔线可调整窗格大小。

**复制模式：** 用方向键 / PageUp / PageDown / Home / End 或 vi 键（`hjkl`）移动；`u` / `d` 半页滚动；`Space` 或 `v` 开始选择；`Enter` 或 `y` 复制（并通过 OSC-52 复制到本地终端的剪贴板）；`q` 或 `Escape` 退出。

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

无需重启即可重载：`lumux source-file <path>`（或前缀键 + `r`）。

## 构建

需要 Rust（stable）。Windows 目标：`x86_64-pc-windows-msvc`。

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
