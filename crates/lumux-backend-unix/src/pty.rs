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
