//! wmux client + CLI entry point.
//!
//! Single multi-call binary: `wmux` is the client and command surface. It can
//! also launch the daemon in-process / auto-spawn it (Phase 7+). The full
//! tmux-ish verb set is filled in across Phases 7 and 9; this establishes the
//! command tree so the workspace builds and `--help` works.

use clap::{Parser, Subcommand};

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
    /// Attach to an existing session.
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
    match cli.command {
        // Phase 7+ wires these verbs to the daemon over the transport.
        Some(Command::New { .. }) => println!("wmux new: not yet implemented (Phase 7)"),
        Some(Command::Attach { .. }) => println!("wmux attach: not yet implemented (Phase 7)"),
        Some(Command::Ls) => println!("wmux ls: not yet implemented (Phase 7)"),
        Some(Command::KillSession { .. }) => println!("wmux kill-session: not yet implemented (Phase 7)"),
        Some(Command::KillServer) => println!("wmux kill-server: not yet implemented (Phase 7)"),
        None => println!("wmux {}: run `wmux --help`", env!("CARGO_PKG_VERSION")),
    }
    Ok(())
}
