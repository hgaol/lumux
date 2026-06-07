//! Server mode: run the background daemon in-process.
//!
//! This is what `wmux --server` executes. It binds the platform listener
//! (Unix socket / Windows named pipe) and runs the control loop from the
//! `wmuxd` library crate. Normally the client re-execs the wmux binary with
//! `--server` to start this detached from any console.

#[cfg(unix)]
pub fn serve() -> anyhow::Result<()> {
    use wmux_backend_unix::{default_socket_path, UnixPtySystem, UnixSocketListener};

    let path = std::env::var_os("WMUX_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);
    tracing::info!(?path, "wmux server binding socket");
    let listener = UnixSocketListener::bind(&path)?;
    let config = wmuxd::load_config();
    wmuxd::run_with_config(UnixPtySystem, listener, config)?;
    tracing::info!("wmux server exiting (no sessions, no clients)");
    Ok(())
}

#[cfg(windows)]
pub fn serve() -> anyhow::Result<()> {
    use wmux_backend_win::{default_pipe_path, PipeListener, WinPtySystem};

    let path = std::env::var("WMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
    tracing::info!(%path, "wmux server binding named pipe");
    let listener = PipeListener::bind(path)?;
    let config = wmuxd::load_config();
    wmuxd::run_with_config(WinPtySystem, listener, config)?;
    tracing::info!("wmux server exiting (no sessions, no clients)");
    Ok(())
}
