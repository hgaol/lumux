//! wmux client + CLI entry point.
//!
//! Single multi-call binary: `wmux` is the client and command surface, and can
//! auto-spawn the daemon. The interactive verbs (new/attach) put the terminal
//! in raw mode and shuttle bytes; the control verbs (ls/kill) send a one-shot
//! command and print the reply.

use clap::{Parser, Subcommand};

#[cfg(unix)]
mod attach;
#[cfg(unix)]
mod control;
#[cfg(unix)]
mod term_unix;

#[derive(Parser)]
#[command(name = "wmux", version, about = "A tmux-like terminal multiplexer for the Windows host")]
struct Cli {
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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    run(cli.command)
}

#[cfg(unix)]
fn run(command: Option<Command>) -> anyhow::Result<()> {
    use wmux_core::proto::Command as Cmd;
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
    }
}

#[cfg(windows)]
fn run(_command: Option<Command>) -> anyhow::Result<()> {
    // Phase 10 wires the Windows client (ConPTY input modes + named pipe).
    anyhow::bail!("wmux: Windows client not yet implemented (Phase 10)")
}
