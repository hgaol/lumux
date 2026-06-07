//! lumux server library.
//!
//! The daemon owns the PTYs and the object tree; it survives client
//! disconnects, which is what gives lumux tmux-style persistence. Exposed as a
//! library so integration tests can drive it in-process against the unix
//! backend.

pub mod daemon;
pub mod eventloop;

pub use daemon::Daemon;
pub use eventloop::{run, run_with_config};

/// Load config from the standard path, if present. Returns the default config
/// when no file exists, and logs (but tolerates) a parse error.
pub fn load_config() -> lumux_core::config::Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match lumux_core::config::Config::from_toml(&text) {
            Ok(cfg) => {
                tracing::info!(?path, "loaded config");
                cfg
            }
            Err(e) => {
                tracing::warn!(?path, error = %e, "config parse error; using defaults");
                lumux_core::config::Config::default()
            }
        },
        Err(_) => lumux_core::config::Config::default(),
    }
}

/// The config file path: $LUMUX_CONFIG, else $XDG_CONFIG_HOME/lumux/config.toml,
/// else ~/.config/lumux/config.toml. On Windows this maps to %APPDATA%\lumux.
pub fn config_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("LUMUX_CONFIG") {
        return p.into();
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return std::path::PathBuf::from(appdata)
                .join("lumux")
                .join("config.toml");
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("lumux").join("config.toml")
}

/// Daemon build/version string, surfaced in the protocol handshake.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
