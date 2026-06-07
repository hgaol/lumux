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
    wmuxd::run(UnixPtySystem, listener)?;
    tracing::info!("wmuxd exiting (no sessions, no clients)");
    Ok(())
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    // Phase 10 wires the ConPTY + named-pipe backend here.
    anyhow::bail!("wmuxd: Windows backend not yet implemented (Phase 10)")
}
