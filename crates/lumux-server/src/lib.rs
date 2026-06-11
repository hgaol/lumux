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

use std::path::Path;

/// Load config from the standard location(s), if present. Returns the default
/// config when no file exists, and logs (but tolerates) a parse error.
///
/// Discovery prefers a tmux-syntax `lumux.conf` (so a copied `~/.tmux.conf`
/// works) and falls back to the native `config.toml`. The first existing
/// candidate wins.
pub fn load_config() -> lumux_core::config::Config {
    for path in config_candidates() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_config(&path, &text) {
            Ok(cfg) => {
                tracing::info!(?path, "loaded config");
                return cfg;
            }
            Err(e) => {
                tracing::warn!(?path, error = %e, "config parse error; using defaults");
                return lumux_core::config::Config::default();
            }
        }
    }
    lumux_core::config::Config::default()
}

/// Parse config text, choosing the format by file extension: `.conf`/`.tmux`
/// (and any non-`.toml` extension) is parsed as tmux syntax; `.toml` as TOML.
/// tmux directives lumux doesn't support are logged as warnings, not errors.
pub fn parse_config(
    path: &Path,
    text: &str,
) -> Result<lumux_core::config::Config, lumux_core::config::ConfigError> {
    let is_toml = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("toml"))
        .unwrap_or(false);
    if is_toml {
        lumux_core::config::Config::from_toml(text)
    } else {
        let parsed = lumux_core::config::Config::from_tmux_verbose(text)?;
        for w in &parsed.warnings {
            tracing::warn!(?path, "tmux config: {w}");
        }
        Ok(parsed.config)
    }
}

/// Ordered list of config paths to try. Honors `$LUMUX_CONFIG` (exact file), then
/// tmux-syntax `lumux.conf`, then native `config.toml`, in both the per-user
/// config dir and the home directory (`~/.lumux.conf`).
pub fn config_candidates() -> Vec<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("LUMUX_CONFIG") {
        return vec![p.into()];
    }
    let mut out = Vec::new();
    // Per-user config directory (%APPDATA%\lumux on Windows, ~/.config/lumux else).
    if let Some(dir) = config_dir() {
        out.push(dir.join("lumux.conf"));
        out.push(dir.join("config.toml"));
    }
    // Home-directory dotfile, mirroring ~/.tmux.conf ergonomics.
    if let Some(home) = home_dir() {
        out.push(home.join(".lumux.conf"));
    }
    out
}

/// The primary config file path (the first candidate). Kept for callers that
/// want a single path to show the user.
pub fn config_path() -> std::path::PathBuf {
    config_candidates()
        .into_iter()
        .next()
        .unwrap_or_else(|| std::path::PathBuf::from("config.toml"))
}

/// The per-user config directory: %APPDATA%\lumux (Windows) or
/// $XDG_CONFIG_HOME/lumux, falling back to ~/.config/lumux.
fn config_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return Some(std::path::PathBuf::from(appdata).join("lumux"));
        }
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))
        .map(|base| base.join("lumux"))
}

/// The user's home directory (USERPROFILE on Windows, HOME elsewhere).
fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("USERPROFILE") {
            return Some(std::path::PathBuf::from(p));
        }
    }
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Daemon build/version string, surfaced in the protocol handshake.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::parse_config;
    use std::path::Path;

    #[test]
    fn dot_conf_is_parsed_as_tmux() {
        let text = "set -g prefix C-a\nset -g mouse on\n";
        let cfg = parse_config(Path::new("lumux.conf"), text).unwrap();
        assert_eq!(cfg.prefix, "C-a");
        assert!(cfg.mouse);
    }

    #[test]
    fn home_dotfile_is_parsed_as_tmux() {
        let text = "set -g history-limit 4096\n";
        let cfg = parse_config(Path::new("/home/u/.lumux.conf"), text).unwrap();
        assert_eq!(cfg.scrollback, 4096);
    }

    #[test]
    fn dot_toml_is_parsed_as_toml() {
        let text = "prefix = \"C-x\"\nmouse = true\n";
        let cfg = parse_config(Path::new("config.toml"), text).unwrap();
        assert_eq!(cfg.prefix, "C-x");
        assert!(cfg.mouse);
    }

    #[test]
    fn toml_syntax_in_a_conf_file_is_treated_as_tmux_and_ignored() {
        // A `.conf` file is tmux syntax; TOML lines aren't tmux commands, so they
        // are skipped (warned) rather than applied — the config stays default.
        let text = "prefix = \"C-x\"\n";
        let cfg = parse_config(Path::new("lumux.conf"), text).unwrap();
        assert_eq!(cfg.prefix, "C-b", "TOML line ignored under tmux parsing");
    }
}
