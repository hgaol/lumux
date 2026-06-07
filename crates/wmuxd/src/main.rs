//! wmux daemon binary entry point.
//!
//! Binds the platform listener and runs the control loop. On Unix it uses the
//! Unix-socket + PTY backend; on Windows (Phase 10) the ConPTY + named-pipe
//! backend. The daemon is normally auto-spawned by the client, detached from
//! any console.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(version = wmuxd::DAEMON_VERSION, "wmuxd starting");
    run()
}

#[cfg(unix)]
fn run() -> anyhow::Result<()> {
    use wmux_backend_unix::{default_socket_path, UnixPtySystem, UnixSocketListener};

    let path = std::env::var_os("WMUX_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);
    tracing::info!(?path, "binding daemon socket");
    let listener = UnixSocketListener::bind(&path)?;
    let config = wmuxd::load_config();
    wmuxd::run_with_config(UnixPtySystem, listener, config)?;
    tracing::info!("wmuxd exiting (no sessions, no clients)");
    Ok(())
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    use wmux_backend_win::{default_pipe_path, PipeListener, WinPtySystem};

    let path = std::env::var("WMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
    tracing::info!(%path, "binding daemon named pipe");
    let listener = PipeListener::bind(path)?;
    let config = wmuxd::load_config();
    wmuxd::run_with_config(WinPtySystem, listener, config)?;
    tracing::info!("wmuxd exiting (no sessions, no clients)");
    Ok(())
}
