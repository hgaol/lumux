//! pi coding-agent lifecycle adapter.
//!
//! pi loads TypeScript extensions from `<agent dir>/extensions/`, so
//! installation is a single file drop with no user config to merge. The
//! extension observes pi's lifecycle events and shells out to
//! `lumux report-state`, the same seam the shell hooks use, so no pi concepts
//! reach the daemon.

use std::ffi::OsString;
use std::path::PathBuf;

use super::common;

const EXTENSION_FILE: &str = "lumux-agent-state.ts";
const EXTENSION_ASSET: &str = include_str!("assets/pi/lumux-agent-state.ts");

pub(super) fn install() -> anyhow::Result<()> {
    install_at(pi_extension_dir()?)
}

fn pi_extension_dir() -> anyhow::Result<PathBuf> {
    pi_extension_dir_with(|key| std::env::var_os(key))
}

/// pi keeps its agent state under `~/.pi/agent`, with extensions in a nested
/// `extensions/` directory.
fn pi_extension_dir_with(getenv: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    let base = match getenv("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = getenv("HOME")
                .filter(|value| !value.is_empty())
                .or_else(|| getenv("USERPROFILE"))
                .ok_or_else(|| anyhow::anyhow!("cannot locate home directory"))?;
            PathBuf::from(home).join(".pi").join("agent")
        }
    };
    Ok(base.join("extensions"))
}

fn install_at(extensions_dir: PathBuf) -> anyhow::Result<()> {
    // Require the agent directory itself to exist: creating the whole tree
    // would silently "succeed" on a machine where pi is not installed.
    let agent_dir = extensions_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid pi extensions path"))?;
    if !agent_dir.is_dir() {
        anyhow::bail!(
            "pi agent directory not found at {}; install the pi coding agent first",
            agent_dir.display()
        );
    }
    std::fs::create_dir_all(&extensions_dir)?;
    let path = extensions_dir.join(EXTENSION_FILE);
    // Not executable: pi imports this, it is never spawned.
    common::write_config_text(&path, EXTENSION_ASSET)?;
    println!("installed the pi state extension at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_dir_prefers_the_env_override() {
        let explicit = pi_extension_dir_with(|key| {
            (key == "PI_CODING_AGENT_DIR").then(|| OsString::from("/tmp/pi-agent"))
        })
        .unwrap();
        assert_eq!(explicit, PathBuf::from("/tmp/pi-agent/extensions"));

        let home = pi_extension_dir_with(|key| (key == "HOME").then(|| OsString::from("/home/u")))
            .unwrap();
        assert_eq!(home, PathBuf::from("/home/u/.pi/agent/extensions"));
    }

    #[test]
    fn install_drops_the_extension_and_is_idempotent() {
        let agent = std::env::temp_dir().join(format!("lumux-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&agent);
        std::fs::create_dir_all(&agent).unwrap();
        let extensions = agent.join("extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        // A user extension alongside must be left alone.
        std::fs::write(extensions.join("mine.ts"), "export default () => {};").unwrap();

        install_at(extensions.clone()).unwrap();
        let path = extensions.join(EXTENSION_FILE);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("LUMUX_INTEGRATION_ID=pi"));
        assert!(
            body.contains("report-state"),
            "the extension must report through the shared CLI seam"
        );
        assert!(
            extensions.join("mine.ts").is_file(),
            "foreign file survives"
        );

        // Re-running overwrites in place rather than accumulating copies.
        install_at(extensions.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(std::fs::read_dir(&extensions).unwrap().count(), 2);

        let _ = std::fs::remove_dir_all(&agent);
    }

    #[test]
    fn install_errors_when_the_agent_dir_is_missing() {
        let agent = std::env::temp_dir().join(format!("lumux-pi-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&agent);
        let err = install_at(agent.join("extensions")).unwrap_err();
        assert!(
            err.to_string()
                .contains("install the pi coding agent first"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extension_only_reports_for_the_ui_session() {
        // A background session must not repaint the pane, so the extension
        // gates on hasUI before claiming the row.
        assert!(EXTENSION_ASSET.contains("hasUI"));
        assert!(EXTENSION_ASSET.contains("rootSession"));
        // Shutting the session down clears the row, since pi's process usually
        // outlives it inside a live shell.
        assert!(EXTENSION_ASSET.contains("session_shutdown"));
        assert!(EXTENSION_ASSET.contains("\"clear\""));
    }
}
