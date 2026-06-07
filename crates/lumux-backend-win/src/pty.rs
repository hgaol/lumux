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
