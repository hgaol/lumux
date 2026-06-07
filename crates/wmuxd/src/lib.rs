//! wmux daemon library.
//!
//! The daemon owns the PTYs and the object tree; it survives client
//! disconnects, which is what gives wmux tmux-style persistence. Exposed as a
//! library so integration tests can drive it in-process against the unix
//! backend.

pub mod daemon;
pub mod eventloop;

pub use daemon::Daemon;
pub use eventloop::{run, run_with_config};

/// Load config from the standard path, if present. Returns the default config
/// when no file exists, and logs (but tolerates) a parse error.
pub fn load_config() -> wmux_core::config::Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match wmux_core::config::Config::from_toml(&text) {
            Ok(cfg) => {
                tracing::info!(?path, "loaded config");
                cfg
            }
            Err(e) => {
                tracing::warn!(?path, error = %e, "config parse error; using defaults");
                wmux_core::config::Config::default()
            }
        },
        Err(_) => wmux_core::config::Config::default(),
    }
}

/// The config file path: $WMUX_CONFIG, else $XDG_CONFIG_HOME/wmux/config.toml,
/// else ~/.config/wmux/config.toml. On Windows this maps to %APPDATA%\wmux.
pub fn config_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("WMUX_CONFIG") {
        return p.into();
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return std::path::PathBuf::from(appdata).join("wmux").join("config.toml");
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("wmux").join("config.toml")
}

/// Daemon build/version string, surfaced in the protocol handshake.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
