//! opencode lifecycle adapter.
//!
//! opencode has no hook-config file: it auto-loads JavaScript plugins from
//! `~/.config/opencode/plugins/`, so installation is a single file drop with no
//! user config to merge — which also means nothing here can conflict with the
//! user's own plugins.
//!
//! The plugin observes opencode's event stream and shells out to
//! `lumux report-state`, the same seam the shell hooks use, so no opencode
//! concepts reach the daemon.

use std::ffi::OsString;
use std::path::PathBuf;

use super::common;

const PLUGIN_FILE: &str = "lumux-agent-state.js";
const PLUGIN_ASSET: &str = include_str!("assets/opencode/lumux-agent-state.js");

pub(super) fn install() -> anyhow::Result<()> {
    install_at(opencode_dir()?)
}

fn opencode_dir() -> anyhow::Result<PathBuf> {
    opencode_dir_with(|key| std::env::var_os(key))
}

/// opencode keeps its config under `~/.config/opencode` (XDG-style) rather than
/// a dotfile in `$HOME`, and honours `$XDG_CONFIG_HOME`.
fn opencode_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = getenv("OPENCODE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = getenv("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("opencode"));
    }
    let home = getenv("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| getenv("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("cannot locate home directory"))?;
    Ok(PathBuf::from(home).join(".config").join("opencode"))
}

fn install_at(dir: PathBuf) -> anyhow::Result<()> {
    if !dir.is_dir() {
        anyhow::bail!(
            "opencode config directory not found at {}; install opencode first",
            dir.display()
        );
    }
    let plugins_dir = dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;
    let plugin_path = plugins_dir.join(PLUGIN_FILE);
    // Not executable: opencode imports this, it is never spawned.
    common::write_config_text(&plugin_path, PLUGIN_ASSET)?;
    println!(
        "installed the opencode state plugin at {}",
        plugin_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_prefers_explicit_then_xdg_then_home() {
        let explicit = opencode_dir_with(|key| {
            (key == "OPENCODE_CONFIG_DIR").then(|| OsString::from("/tmp/oc"))
        })
        .unwrap();
        assert_eq!(explicit, PathBuf::from("/tmp/oc"));

        let xdg = opencode_dir_with(|key| {
            (key == "XDG_CONFIG_HOME").then(|| OsString::from("/tmp/xdg"))
        })
        .unwrap();
        assert_eq!(xdg, PathBuf::from("/tmp/xdg/opencode"));

        let home =
            opencode_dir_with(|key| (key == "HOME").then(|| OsString::from("/home/u"))).unwrap();
        assert_eq!(home, PathBuf::from("/home/u/.config/opencode"));
    }

    #[test]
    fn install_drops_the_plugin_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("lumux-opencode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A user plugin sitting alongside must be left alone.
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("mine.js"), "export const Mine = () => ({});").unwrap();

        install_at(dir.clone()).unwrap();
        let plugin = plugins.join(PLUGIN_FILE);
        let body = std::fs::read_to_string(&plugin).unwrap();
        // The asset carries the managed-file marker in its header.
        assert!(body.contains("LUMUX_INTEGRATION_ID=opencode"));
        assert!(
            body.contains("report-state"),
            "the plugin must report through the shared CLI seam"
        );
        assert!(plugins.join("mine.js").is_file(), "foreign plugin survives");

        // Re-running overwrites in place rather than accumulating copies.
        install_at(dir.clone()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            body,
            "reinstall must be byte-identical"
        );
        let count = std::fs::read_dir(&plugins).unwrap().count();
        assert_eq!(count, 2, "exactly our plugin plus the user's");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_errors_when_the_config_dir_is_missing() {
        let dir = std::env::temp_dir().join(format!("lumux-oc-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = install_at(dir).unwrap_err();
        assert!(
            err.to_string().contains("install opencode first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plugin_drops_subagent_sessions() {
        // Subagent sessions carry a parentID; letting their lifecycle through
        // would repaint the pane with a background task's state.
        assert!(PLUGIN_ASSET.contains("parentID"));
        assert!(PLUGIN_ASSET.contains("childSessions"));
    }
}
