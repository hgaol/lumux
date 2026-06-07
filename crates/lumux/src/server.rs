//! Server mode: run the background daemon in-process.
//!
//! This is what `lumux --server` executes. It binds the platform listener
//! (Unix socket / Windows named pipe) and runs the control loop from the
//! `lumux_server` library crate. Normally the client re-execs the lumux binary with
//! `--server` to start this detached from any console.

#[cfg(unix)]
pub fn serve() -> anyhow::Result<()> {
    use lumux_backend_unix::{default_socket_path, UnixPtySystem, UnixSocketListener};

    let path = std::env::var_os("LUMUX_SOCK")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);
    tracing::info!(?path, "lumux server binding socket");
    let listener = UnixSocketListener::bind(&path)?;
    let config = lumux_server::load_config();
    lumux_server::run_with_config(UnixPtySystem, listener, config)?;
    tracing::info!("lumux server exiting (no sessions, no clients)");
    Ok(())
}

#[cfg(windows)]
pub fn serve() -> anyhow::Result<()> {
    use lumux_backend_win::{default_pipe_path, PipeListener, WinPtySystem};

    let path = std::env::var("LUMUX_PIPE").unwrap_or_else(|_| default_pipe_path());
    tracing::info!(%path, "lumux server binding named pipe");
    let listener = PipeListener::bind(path)?;
    let config = lumux_server::load_config();
    lumux_server::run_with_config(WinPtySystem, listener, config)?;
    tracing::info!("lumux server exiting (no sessions, no clients)");
    Ok(())
}
