//! On-disk session snapshots (tmux-resurrect style).
//!
//! lumux can save the *structure* of its sessions — windows, the pane split
//! layout, and each pane's shell argv + working directory — to disk and rebuild
//! it when the daemon restarts. Running programs are NOT resurrected (they are
//! live PTY children); only the shell is relaunched in the saved directory, the
//! same trade-off tmux-resurrect makes.
//!
//! These DTOs are deliberately separate from the live [`crate::model`] types:
//! the model has private fields and runtime-only state (zoom, last-pane, the
//! preset layout kind) that must not leak into the saved format. Pane ids in the
//! snapshot are *snapshot-local* dense `u32`s, remapped on restore to freshly
//! allocated [`PaneId`](crate::model::PaneId)s, so the file never depends on the
//! daemon's live id counter.

use serde::{Deserialize, Serialize};

use crate::model::PaneNode;

/// Bumped whenever the on-disk layout changes incompatibly. A file with a
/// different version is ignored (the daemon starts fresh) rather than
/// mis-parsed.
pub const STATE_VERSION: u32 = 1;

/// The whole saved state: every session, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    pub sessions: Vec<SessionSnap>,
}

/// One saved session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnap {
    pub name: String,
    pub windows: Vec<WindowSnap>,
    /// Index into `windows` of the active window (clamped on restore).
    pub active_window: usize,
}

/// One saved window: its split layout plus the panes it references. The
/// `layout` tree's `PaneNode::Leaf` ids are snapshot-local indices into `panes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub name: String,
    pub layout: PaneNode,
    pub panes: Vec<PaneSnap>,
    /// Index into `panes` of the active pane (clamped on restore).
    pub active_pane: usize,
    pub synchronized: bool,
    pub auto_rename: bool,
}

/// One saved pane: enough to relaunch its shell where it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSnap {
    /// Snapshot-local id; matches the `PaneNode::Leaf(PaneId(layout_id))` in the
    /// window's `layout` tree.
    pub layout_id: u32,
    /// The argv used to (re)spawn the pane's shell.
    pub shell: Vec<String>,
    /// The pane's working directory at save time, if it could be resolved.
    pub cwd: Option<String>,
}

impl StateFile {
    pub fn new(sessions: Vec<SessionSnap>) -> Self {
        Self {
            version: STATE_VERSION,
            sessions,
        }
    }

    /// Whether there is anything worth saving.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Serialize to bytes (bincode). Used by the daemon to write the state file.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        bincode::serialize(self).map_err(|e| e.to_string())
    }

    /// Deserialize from bytes, returning None when the data is corrupt OR carries
    /// a different [`STATE_VERSION`] — in both cases the caller starts fresh
    /// rather than acting on state it can't trust.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let sf: StateFile = bincode::deserialize(bytes).ok()?;
        if sf.version != STATE_VERSION {
            return None;
        }
        Some(sf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PaneId, SplitDir};

    fn sample() -> StateFile {
        // A window whose layout is [0 | [1 / 2]] over three panes.
        let mut layout = PaneNode::leaf(PaneId(0));
        layout.split_leaf(PaneId(0), PaneId(1), SplitDir::Horizontal);
        layout.split_leaf(PaneId(1), PaneId(2), SplitDir::Vertical);
        let panes = vec![
            PaneSnap { layout_id: 0, shell: vec!["/bin/sh".into()], cwd: Some("/tmp".into()) },
            PaneSnap { layout_id: 1, shell: vec!["/bin/bash".into()], cwd: None },
            PaneSnap { layout_id: 2, shell: vec!["/bin/sh".into()], cwd: Some("/".into()) },
        ];
        let win = WindowSnap {
            name: "editor".into(),
            layout,
            panes,
            active_pane: 2,
            synchronized: false,
            auto_rename: true,
        };
        StateFile::new(vec![SessionSnap {
            name: "work".into(),
            windows: vec![win],
            active_window: 0,
        }])
    }

    #[test]
    fn round_trips_through_bincode() {
        let sf = sample();
        let bytes = sf.encode().unwrap();
        let back = StateFile::decode(&bytes).expect("decodes");
        assert_eq!(sf, back, "snapshot must round-trip exactly");
        // Spot-check the structure survived.
        assert_eq!(back.sessions.len(), 1);
        let w = &back.sessions[0].windows[0];
        assert_eq!(w.name, "editor");
        assert_eq!(w.panes.len(), 3);
        assert_eq!(w.layout.pane_count(), 3);
        assert_eq!(w.layout.pane_ids(), vec![PaneId(0), PaneId(1), PaneId(2)]);
        assert_eq!(w.panes[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(w.active_pane, 2);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let mut sf = sample();
        sf.version = STATE_VERSION + 99;
        let bytes = bincode::serialize(&sf).unwrap();
        assert!(StateFile::decode(&bytes).is_none(), "a future version must be ignored");
    }

    #[test]
    fn garbage_decodes_to_none() {
        assert!(StateFile::decode(b"not a valid state file").is_none());
    }
}
