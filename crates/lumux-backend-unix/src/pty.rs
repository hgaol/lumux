//! Real Unix PTY backend via `portable-pty`.
//!
//! This is the development/CI substrate: it spawns actual shells under a Unix
//! pseudo-terminal so the whole daemon can be exercised on Linux. The Windows
//! backend (Phase 10) implements the same `lumux_core` traits over ConPTY.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use lumux_core::traits::{Pty, PtySize, PtySystem, PtyWriter, ShellCommand};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize as PpSize};

fn to_pp(size: PtySize) -> PpSize {
    PpSize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// A spawned Unix PTY: master handle + child process, plus a cloned reader.
pub struct UnixPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

/// Input/control half: owns the master (for resize) and child (for wait/kill)
/// behind a shared lock so the daemon's writer task and reaper can both reach
/// it.
pub struct UnixPtyWriter {
    inner: Arc<Mutex<WriterInner>>,
}

struct WriterInner {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
}

impl Pty for UnixPty {
    type Writer = UnixPtyWriter;
    type Reader = Box<dyn Read + Send>;

    fn split(mut self) -> std::io::Result<(Self::Writer, Self::Reader)> {
        let reader = self
            .reader
            .take()
            .ok_or_else(|| std::io::Error::other("reader already taken"))?;
        let writer = UnixPtyWriter {
            inner: Arc::new(Mutex::new(WriterInner {
                master: self.master,
                child: self.child,
                writer: self.writer,
            })),
        };
        Ok((writer, reader))
    }
}

impl PtyWriter for UnixPtyWriter {
    fn write_input(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.writer.write_all(data)?;
        g.writer.flush()
    }

    fn resize(&mut self, size: PtySize) -> std::io::Result<()> {
        let g = self.inner.lock().unwrap();
        g.master
            .resize(to_pp(size))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        let mut g = self.inner.lock().unwrap();
        match g.child.try_wait()? {
            Some(status) => Ok(Some(status.exit_code() as i32)),
            None => Ok(None),
        }
    }

    fn child_pid(&self) -> Option<u32> {
        self.inner.lock().unwrap().child.process_id()
    }
}

/// Spawns [`UnixPty`]s.
pub struct UnixPtySystem;

impl PtySystem for UnixPtySystem {
    type Pty = UnixPty;

    fn descendant_process_names(&self, child_pid: u32) -> Vec<String> {
        Self::descendant_names(child_pid)
    }

    fn spawn(&self, cmd: &ShellCommand, size: PtySize) -> std::io::Result<Self::Pty> {
        let pair = native_pty_system()
            .openpty(to_pp(size))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let (prog, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| std::io::Error::other("empty argv"))?;
        let mut builder = CommandBuilder::new(prog);
        builder.args(args);
        if let Some(cwd) = &cmd.cwd {
            builder.cwd(cwd);
        }
        for (k, v) in &cmd.env {
            builder.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // The slave is no longer needed in this process once the child holds it.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(UnixPty {
            master: pair.master,
            child,
            reader: Some(reader),
            writer,
        })
    }
}

/// Read `/proc` once and return `(pid, ppid, comm)` for every process.
///
/// A single directory pass keeps the per-tick cost independent of the pane
/// count. Unreadable or vanished entries are skipped rather than failing the
/// whole scan — processes come and go while we walk.
#[cfg(target_os = "linux")]
fn process_table() -> Vec<(u32, u32, String)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read(entry.path().join("stat")) else {
            continue;
        };
        // `stat` is "pid (comm) state ppid ...". The comm field may itself
        // contain spaces or parentheses, so split on the LAST ')'.
        let Some(close) = stat.iter().rposition(|&b| b == b')') else {
            continue;
        };
        let Some(open) = stat.iter().position(|&b| b == b'(') else {
            continue;
        };
        if open + 1 > close {
            continue;
        }
        let comm = String::from_utf8_lossy(&stat[open + 1..close]).into_owned();
        let rest = String::from_utf8_lossy(&stat[close + 1..]).into_owned();
        // After the comm: " state ppid ..." — ppid is the 2nd whitespace field.
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        out.push((pid, ppid, comm));
    }
    out
}

/// Every process name in the subtree rooted at `root` (excluding `root` itself,
/// which is the pane's shell).
#[cfg(target_os = "linux")]
fn descendants_of(root: u32, table: &[(u32, u32, String)]) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut by_parent: HashMap<u32, Vec<(u32, &str)>> = HashMap::new();
    for (pid, ppid, comm) in table {
        by_parent.entry(*ppid).or_default().push((*pid, comm));
    }
    let mut names = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.push_back(root);
    seen.insert(root);
    while let Some(pid) = queue.pop_front() {
        let Some(children) = by_parent.get(&pid) else {
            continue;
        };
        for (child, comm) in children {
            // A malformed table can't loop us forever.
            if !seen.insert(*child) {
                continue;
            }
            names.push((*comm).to_string());
            queue.push_back(*child);
        }
    }
    names
}

impl UnixPtySystem {
    #[cfg(target_os = "linux")]
    fn descendant_names(child_pid: u32) -> Vec<String> {
        descendants_of(child_pid, &process_table())
    }

    /// macOS and other unixes have no `/proc`; shell out to `ps` for the
    /// pid/ppid/comm table. Bounded and best-effort — a failure just means no
    /// detection, never a false "exited".
    #[cfg(not(target_os = "linux"))]
    fn descendant_names(child_pid: u32) -> Vec<String> {
        use std::process::{Command, Stdio};
        let Ok(out) = Command::new("ps")
            .args(["-Ao", "pid=,ppid=,comm="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut table = Vec::new();
        for line in text.lines() {
            let mut f = line.split_whitespace();
            let (Some(pid), Some(ppid), Some(comm)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            // `comm` is a path on macOS; keep the basename so it matches the
            // Linux `comm` shape.
            let base = comm.rsplit('/').next().unwrap_or(comm).to_string();
            table.push((pid, ppid, base));
        }
        descendants_of_generic(child_pid, &table)
    }
}

/// Portable subtree walk shared by the non-Linux path.
#[cfg(not(target_os = "linux"))]
fn descendants_of_generic(root: u32, table: &[(u32, u32, String)]) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut by_parent: HashMap<u32, Vec<(u32, &str)>> = HashMap::new();
    for (pid, ppid, comm) in table {
        by_parent.entry(*ppid).or_default().push((*pid, comm));
    }
    let mut names = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.push_back(root);
    seen.insert(root);
    while let Some(pid) = queue.pop_front() {
        let Some(children) = by_parent.get(&pid) else {
            continue;
        };
        for (child, comm) in children {
            if !seen.insert(*child) {
                continue;
            }
            names.push((*comm).to_string());
            queue.push_back(*child);
        }
    }
    names
}

#[cfg(test)]
mod descendant_tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn walks_a_synthetic_subtree() {
        let table = vec![
            (1, 0, "init".to_string()),
            (10, 1, "sh".to_string()),
            (11, 10, "node".to_string()),
            (12, 11, "codex".to_string()),
            (20, 1, "other".to_string()),
        ];
        let mut names = super::descendants_of(10, &table);
        names.sort();
        assert_eq!(names, vec!["codex".to_string(), "node".to_string()]);
        // A pane with no children detects nothing.
        assert!(super::descendants_of(20, &table).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_parent_cycle_cannot_hang_the_walk() {
        // Malformed table where two pids claim each other as parent.
        let table = vec![
            (10, 1, "sh".to_string()),
            (11, 10, "a".to_string()),
            (10, 11, "loop".to_string()),
        ];
        let names = super::descendants_of(10, &table);
        assert!(names.len() < 5, "walk must terminate, got {names:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_real_process_table() {
        // Sanity: our own process must appear with a plausible parent.
        let table = super::process_table();
        assert!(!table.is_empty(), "/proc scan returned nothing");
        let me = std::process::id();
        assert!(table.iter().any(|(pid, _, _)| *pid == me));
    }
}
