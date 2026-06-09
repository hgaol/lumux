//! lumux entry point — a single multi-call binary, like tmux.
//!
//! `lumux` is the client and command surface, and it *is* also the server: when
//! no daemon is running, the client re-execs this same binary with a hidden
//! `--server` flag to spawn the background daemon (detached from any console).
//! The interactive verbs (new/attach) put the terminal in raw mode and shuttle
//! bytes; the control verbs (ls/kill) send a one-shot command and print the
//! reply.

use clap::{Parser, Subcommand};

mod attach;
mod control;
mod server;
#[cfg(unix)]
mod term_unix;
#[cfg(windows)]
mod term_win;

#[derive(Parser)]
#[command(
    name = "lumux",
    version,
    about = "A tmux-like terminal multiplexer for the Windows host"
)]
struct Cli {
    /// Internal: run as the background daemon. Not meant to be invoked directly;
    /// the client re-execs itself with this flag to start the server.
    #[arg(long = "server", hide = true)]
    server: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new session and attach to it.
    New {
        #[arg(short = 's', long)]
        session: Option<String>,
        #[arg(short = 'n', long)]
        window: Option<String>,
        #[arg(long)]
        shell: Option<String>,
    },
    /// Attach to an existing session (creating a default if none exists).
    Attach {
        #[arg(short = 't', long)]
        target: Option<String>,
    },
    /// List sessions.
    Ls,
    /// Kill a session.
    KillSession {
        #[arg(short = 't', long)]
        target: String,
    },
    /// Kill the daemon and all sessions.
    KillServer,
    /// Split the active window of a session (sends a command to the daemon).
    // `disable_help_flag` frees up `-h` so it can mean horizontal (tmux parity)
    // instead of clap's auto help alias; `lumux help split-window` still works.
    #[command(disable_help_flag = true)]
    SplitWindow {
        /// Split left/right (vertical divider) instead of top/bottom.
        #[arg(short = 'h', long)]
        horizontal: bool,
    },
    /// Create a new window in the current session.
    NewWindow,
    /// Send literal keystrokes to the active pane.
    SendKeys {
        /// The keys to send (sent verbatim, with a trailing newline added).
        keys: String,
    },
    /// Rename the active window of the current session.
    RenameWindow {
        /// The new window name.
        name: String,
    },
    /// Rename the current session.
    RenameSession {
        /// The new session name.
        name: String,
    },
    /// Reload configuration from a TOML file.
    SourceFile { path: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Server mode: become the background daemon. Uses a chattier log level since
    // there's no interactive terminal to disturb.
    if cli.server {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        return server::serve();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    run(cli.command)
}

fn run(command: Option<Command>) -> anyhow::Result<()> {
    use lumux_core::proto::Command as Cmd;
    match command {
        Some(Command::New { session, shell, .. }) => attach::attach(session, true, shell),
        Some(Command::Attach { target }) => attach::attach(target, false, None),
        None => attach::attach(None, false, None),
        Some(Command::Ls) => {
            let reply = control::send_command(Cmd::ListSessions)?;
            print!("{reply}");
            Ok(())
        }
        Some(Command::KillSession { target }) => {
            control::send_command(Cmd::KillSession { target })?;
            Ok(())
        }
        Some(Command::KillServer) => {
            control::send_command(Cmd::KillServer)?;
            Ok(())
        }
        Some(Command::SplitWindow { horizontal }) => {
            control::send_command(Cmd::SplitWindow { horizontal })?;
            Ok(())
        }
        Some(Command::NewWindow) => {
            control::send_command(Cmd::NewWindow { name: None })?;
            Ok(())
        }
        Some(Command::SendKeys { keys }) => {
            let mut bytes = keys.into_bytes();
            bytes.push(b'\n');
            control::send_command(Cmd::SendKeys { keys: bytes })?;
            Ok(())
        }
        Some(Command::RenameWindow { name }) => {
            control::send_command(Cmd::RenameWindow { name })?;
            Ok(())
        }
        Some(Command::RenameSession { name }) => {
            control::send_command(Cmd::RenameSession { name })?;
            Ok(())
        }
        Some(Command::SourceFile { path }) => {
            let reply = control::send_command(Cmd::SourceFile { path })?;
            print!("{reply}");
            Ok(())
        }
    }
}
