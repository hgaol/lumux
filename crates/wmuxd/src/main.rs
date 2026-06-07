//! wmux daemon binary entry point.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(version = wmuxd::DAEMON_VERSION, "{}", wmuxd::describe());
    // Phase 7: build the backend listener + event loop and run it here.
    Ok(())
}
