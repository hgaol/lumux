//! Shared mechanics for agent integration adapters.
//!
//! Provider modules own their native hook schema and lifecycle mapping. This
//! module owns only the invariants that must be identical for every adapter:
//! config-root discovery, symlink-safe target resolution, and atomic writes.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Resolve an agent config directory from its override or the user's home.
/// Empty overrides fall back to `HOME`/`USERPROFILE`; a leading `~` is expanded
/// without requiring the path to exist yet.
pub(super) fn config_dir_with(
    env_var: &str,
    default_dir: &str,
    getenv: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<PathBuf> {
    let home = || {
        getenv("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| getenv("USERPROFILE").filter(|value| !value.is_empty()))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("cannot locate home directory"))
    };

    let Some(override_dir) = getenv(env_var).filter(|value| !value.is_empty()) else {
        return Ok(home()?.join(default_dir));
    };
    let path = PathBuf::from(override_dir);
    let Some(text) = path.to_str() else {
        return Ok(path);
    };
    if text == "~" {
        return home();
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return Ok(home()?.join(rest));
    }
    Ok(path)
}

/// Resolve only the final file's symlink chain so an atomic rename updates the
/// linked target without replacing the user's link. Missing final targets are
/// valid; inspection failures and cycles are rejected before any mutation.
pub(super) fn resolve_write_path(path: &Path) -> anyhow::Result<PathBuf> {
    const MAX_SYMLINKS: usize = 64;

    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| anyhow::anyhow!("failed to resolve {}: {error}", path.display()))?
            .join(path)
    };
    let mut seen = HashSet::new();

    for _ in 0..MAX_SYMLINKS {
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "failed to inspect {}: {error}",
                    current.display()
                ));
            }
        };
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }

        // Canonicalize the containing directory, not the link itself. This
        // preserves POSIX resolution semantics for relative targets containing
        // `..` below a symlinked directory.
        let parent = current.parent().unwrap_or_else(|| Path::new("."));
        let file_name = current
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("symlink {} does not name a file", current.display()))?;
        let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
            anyhow::anyhow!("failed to resolve directory {}: {error}", parent.display())
        })?;
        let identity = canonical_parent.join(file_name);
        if !seen.insert(identity.clone()) {
            anyhow::bail!(
                "refusing to update {}: symlink cycle reaches {}",
                path.display(),
                identity.display()
            );
        }

        let target = std::fs::read_link(&current).map_err(|error| {
            anyhow::anyhow!("failed to read symlink {}: {error}", current.display())
        })?;
        current = if target.is_absolute() {
            target
        } else {
            parent.join(target)
        };
    }

    anyhow::bail!(
        "refusing to update {}: symlink chain exceeds {MAX_SYMLINKS} links or is cyclic",
        path.display()
    )
}

/// Read a JSON config through its resolved write target. Empty and missing
/// files start as an object; malformed or unreadable files remain untouched.
pub(super) fn read_json_config(path: &Path) -> anyhow::Result<(PathBuf, serde_json::Value)> {
    let write_path = resolve_write_path(path)?;
    let root = match std::fs::read_to_string(&write_path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("{} is not valid JSON: {error}", path.display()))?,
        Ok(_) => serde_json::json!({}),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to read {}: {error}",
                path.display()
            ));
        }
    };
    Ok((write_path, root))
}

/// Serialize and atomically replace a JSON config beside its resolved target.
pub(super) fn write_json_config(
    display_path: &Path,
    write_path: &Path,
    root: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(root)?;
    bytes.push(b'\n');
    atomic_write(display_path, write_path, &bytes, false)
}

/// Remove provider-owned commands from nested JSON hook groups while
/// preserving every piece of foreign or structurally unknown configuration.
/// A group is removed only when this operation makes its command list empty;
/// an event is removed only when managed-command deletion empties all groups.
pub(super) fn prune_managed_nested_hooks(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    is_managed: impl Fn(&serde_json::Value) -> bool,
) {
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks
            .get_mut(&event)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };

        let mut removed_any = false;
        groups.retain_mut(|group| {
            let Some(commands) = group
                .get_mut("hooks")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return true;
            };
            let before = commands.len();
            commands.retain(|command| !is_managed(command));
            let removed = commands.len() != before;
            removed_any |= removed;

            // Pre-existing empty or unknown groups belong to the user. Only
            // deleting a managed command gives us ownership of group removal.
            !(removed && commands.is_empty())
        });

        if removed_any && groups.is_empty() {
            hooks.remove(&event);
        }
    }
}

/// Atomically write a UTF-8 managed hook, following a pre-existing final
/// symlink while retaining that link. Unix hooks are made executable.
pub(super) fn write_managed_hook(path: &Path, content: &str) -> anyhow::Result<()> {
    let write_path = resolve_write_path(path)?;
    atomic_write(path, &write_path, content.as_bytes(), true)
}

/// Atomically write arbitrary config text through its final symlink target.
pub(super) fn write_config_text(path: &Path, content: &str) -> anyhow::Result<()> {
    let write_path = resolve_write_path(path)?;
    atomic_write(path, &write_path, content.as_bytes(), false)
}

fn atomic_write(
    display_path: &Path,
    write_path: &Path,
    bytes: &[u8],
    executable: bool,
) -> anyhow::Result<()> {
    let parent = write_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        anyhow::anyhow!("failed to create directory {}: {error}", parent.display())
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        anyhow::anyhow!(
            "failed to create a temporary file beside {}: {error}",
            display_path.display()
        )
    })?;
    temporary
        .write_all(bytes)
        .map_err(|error| anyhow::anyhow!("failed to write {}: {error}", display_path.display()))?;

    // Preserve permissions when replacing user-owned config. Managed Unix
    // scripts always need execute bits, even on their first installation.
    if let Ok(metadata) = std::fs::metadata(write_path) {
        std::fs::set_permissions(temporary.path(), metadata.permissions()).map_err(|error| {
            anyhow::anyhow!(
                "failed to preserve permissions for {}: {error}",
                display_path.display()
            )
        })?;
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(temporary.path())?.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        std::fs::set_permissions(temporary.path(), permissions)?;
    }
    let _ = executable;

    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| anyhow::anyhow!("failed to sync {}: {error}", display_path.display()))?;
    temporary.persist(write_path).map_err(|error| {
        anyhow::anyhow!(
            "failed to atomically replace {}: {}",
            display_path.display(),
            error.error
        )
    })?;

    // Durably record the rename where the platform permits syncing a directory.
    #[cfg(unix)]
    {
        let directory = std::fs::File::open(parent).map_err(|error| {
            anyhow::anyhow!("failed to open directory {}: {error}", parent.display())
        })?;
        directory.sync_all().map_err(|error| {
            anyhow::anyhow!("failed to sync directory {}: {error}", parent.display())
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn managed_nested_hook_pruning_preserves_foreign_and_unknown_structure() {
        let mut hooks = serde_json::json!({
            "Mixed": [
                {
                    "matcher": "keep-metadata",
                    "extra": 7,
                    "hooks": [
                        {"owner": "lumux"},
                        {"command": "foreign"}
                    ]
                },
                {"matcher": "managed-only", "hooks": [{"owner": "lumux"}]},
                {"matcher": "already-empty", "hooks": []},
                {"matcher": "unknown-group"},
                {"matcher": "wrong-hook-shape", "hooks": {"keep": true}}
            ],
            "ManagedOnly": [{"metadata": "discard-with-group", "hooks": [{"owner": "lumux"}]}],
            "AlreadyEmpty": [],
            "WrongEventShape": {"keep": true}
        })
        .as_object()
        .unwrap()
        .clone();

        prune_managed_nested_hooks(&mut hooks, |command| command["owner"] == "lumux");

        assert_eq!(
            Value::Object(hooks),
            serde_json::json!({
                "Mixed": [
                    {
                        "matcher": "keep-metadata",
                        "extra": 7,
                        "hooks": [{"command": "foreign"}]
                    },
                    {"matcher": "already-empty", "hooks": []},
                    {"matcher": "unknown-group"},
                    {"matcher": "wrong-hook-shape", "hooks": {"keep": true}}
                ],
                "AlreadyEmpty": [],
                "WrongEventShape": {"keep": true}
            })
        );
    }
}
