//! Windows PTY backend via `portable-pty` (ConPTY path).
//!
//! `portable_pty::native_pty_system()` resolves to the ConPTY implementation on
//! Windows (CreatePseudoConsole / ResizePseudoConsole), so this is structurally
//! identical to the unix backend — the platform difference is entirely inside
//! portable-pty. ConPTY also auto-translates legacy console apps (cmd, PS 5.x)
//! into VT output, which is what makes lumux shell-agnostic on Windows.
//!
//! NOTE: built and type-checked from Linux via the msvc target; real ConPTY
//! behavior is exercised on Windows CI (Phase 10/11).

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

pub struct WinPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

pub struct WinPtyWriter {
    inner: Arc<Mutex<WriterInner>>,
}

struct WriterInner {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
}

impl Pty for WinPty {
    type Writer = WinPtyWriter;
    type Reader = Box<dyn Read + Send>;

    fn split(mut self) -> std::io::Result<(Self::Writer, Self::Reader)> {
        let reader = self
            .reader
            .take()
            .ok_or_else(|| std::io::Error::other("reader already taken"))?;
        let writer = WinPtyWriter {
            inner: Arc::new(Mutex::new(WriterInner {
                master: self.master,
                child: self.child,
                writer: self.writer,
            })),
        };
        Ok((writer, reader))
    }
}

impl PtyWriter for WinPtyWriter {
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

pub struct WinPtySystem;

impl PtySystem for WinPtySystem {
    type Pty = WinPty;

    #[cfg(windows)]
    fn descendant_process_names(&self, child_pid: u32) -> Vec<String> {
        descendants_of(child_pid, &process_table())
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
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(WinPty {
            master: pair.master,
            child,
            reader: Some(reader),
            writer,
        })
    }
}

/// Snapshot every process as `(pid, ppid, exe_name)` via the Toolhelp API.
///
/// One snapshot per call keeps the cost independent of the pane count. A failed
/// snapshot yields an empty table, which callers must read as "unknown" rather
/// than "nothing is running".
#[cfg(windows)]
fn process_table() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        if Process32First(snapshot, &mut entry) != 0 {
            loop {
                // szExeFile is a NUL-terminated ANSI name like "codex.exe".
                // Win32 `CHAR` is i8, so reinterpret before decoding.
                let raw = &entry.szExeFile;
                let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                let bytes: Vec<u8> = raw[..len].iter().map(|&c| c as u8).collect();
                let name = String::from_utf8_lossy(&bytes).into_owned();
                out.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    out
}

/// Every process name in the subtree rooted at `root` (excluding `root`).
#[cfg(windows)]
fn descendants_of(root: u32, table: &[(u32, u32, String)]) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut by_parent: HashMap<u32, Vec<(u32, &str)>> = HashMap::new();
    for (pid, ppid, name) in table {
        by_parent.entry(*ppid).or_default().push((*pid, name));
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
        for (child, name) in children {
            // Windows reuses pids, so a stale snapshot can describe a cycle.
            if !seen.insert(*child) {
                continue;
            }
            names.push((*name).to_string());
            queue.push_back(*child);
        }
    }
    names
}

#[cfg(all(test, windows))]
mod descendant_tests {
    #[test]
    fn walks_a_synthetic_subtree() {
        let table = vec![
            (10, 1, "cmd.exe".to_string()),
            (11, 10, "node.exe".to_string()),
            (12, 11, "codex.exe".to_string()),
            (20, 1, "other.exe".to_string()),
        ];
        let mut names = super::descendants_of(10, &table);
        names.sort();
        assert_eq!(
            names,
            vec!["codex.exe".to_string(), "node.exe".to_string()]
        );
        assert!(super::descendants_of(20, &table).is_empty());
    }

    #[test]
    fn a_parent_cycle_cannot_hang_the_walk() {
        let table = vec![
            (10, 1, "cmd.exe".to_string()),
            (11, 10, "a.exe".to_string()),
            (10, 11, "loop.exe".to_string()),
        ];
        assert!(super::descendants_of(10, &table).len() < 5);
    }
}
