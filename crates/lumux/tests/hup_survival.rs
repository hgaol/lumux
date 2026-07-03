//! Regression test: the daemon must survive the client's controlling terminal
//! going away (SSH disconnect). On Unix that means the client SIGHUPs its
//! process group; the daemon is spawned with `setsid()` into its own session so
//! it does NOT receive that hangup. Without the fix, the last session dies with
//! the SSH connection.
//!
//! Unix-only: the whole scenario is about POSIX sessions / SIGHUP.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Whether something is accepting connections on `sock` (i.e. the daemon lives).
fn daemon_alive(sock: &str) -> bool {
    UnixStream::connect(sock).is_ok()
}

#[test]
fn daemon_survives_client_group_sighup() {
    let bin = env!("CARGO_BIN_EXE_lumux");
    let pid = std::process::id();
    let sock = format!("/tmp/lumux-huptest-{pid}.sock");
    let _ = std::fs::remove_file(&sock);

    // Fork a child that becomes a session leader with a pty as its controlling
    // terminal — exactly the shape of an sshd login shell — then execs the
    // client to create a session. This mirrors how a real SSH session hosts the
    // `lumux` client.
    let (master_fd, child) = unsafe {
        let mut master: libc::c_int = 0;
        let cpid = libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        );
        (master, cpid)
    };
    assert!(child >= 0, "forkpty failed");

    if child == 0 {
        // Child: forkpty already put us in a new session with the slave pty as
        // the controlling terminal. Exec the client to create a session.
        use std::ffi::CString;
        let prog = CString::new(bin).unwrap();
        let args = [
            CString::new("lumux").unwrap(),
            CString::new("new").unwrap(),
            CString::new("-s").unwrap(),
            CString::new("hup").unwrap(),
            CString::new("--shell").unwrap(),
            CString::new("/bin/sh").unwrap(),
        ];
        // Point the client at our test socket + no state file, inheriting env.
        std::env::set_var("LUMUX_SOCK", &sock);
        std::env::remove_var("LUMUX_STATE");
        // Clear the nested-session guard: if this test itself runs inside a lumux
        // session, `$LUMUX` would make the client refuse to start a new one.
        std::env::remove_var("LUMUX");
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        unsafe {
            libc::execv(prog.as_ptr(), argv.as_ptr());
        }
        // execv only returns on failure.
        unsafe { libc::_exit(127) };
    }

    // Parent: drain the pty for a bit so the client attaches and the daemon
    // spawns and creates the session.
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    // Non-blocking-ish drain with a deadline; we just need to let it run.
    drain_until(&mut master, Duration::from_secs(3), &sock, true);

    assert!(
        daemon_alive(&sock),
        "precondition: the daemon should be running after the client created a session"
    );

    // Simulate the SSH disconnect: SIGHUP the client's process group (what the
    // kernel does when the controlling terminal is lost), then close the pty.
    unsafe {
        let pgid = libc::getpgid(child);
        if pgid > 0 {
            libc::killpg(pgid, libc::SIGHUP);
        }
        libc::kill(child, libc::SIGHUP);
    }
    drop(master); // close the pty master (drops the controlling terminal)
    // Reap the child so it doesn't linger as a zombie.
    unsafe {
        let mut status = 0;
        libc::waitpid(child, &mut status, 0);
    }

    // Give any (incorrectly) propagated signal time to land.
    std::thread::sleep(Duration::from_millis(800));

    let alive = daemon_alive(&sock);
    // Clean up: if it's alive, tell it to shut down so we don't leak a daemon.
    if alive {
        let _ = std::fs::remove_file(&sock);
    }
    assert!(
        alive,
        "the daemon must SURVIVE the SSH-disconnect SIGHUP (setsid detaches it \
         into its own session); the last session was lost otherwise"
    );
}

/// Read+discard from `f` until `deadline`, optionally stopping early once the
/// socket appears. Keeps the client's pty from filling up while it starts.
fn drain_until(f: &mut std::fs::File, dur: Duration, sock: &str, stop_when_up: bool) {
    // Best-effort: set the fd non-blocking so reads don't wedge the test.
    unsafe {
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(f);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let end = Instant::now() + dur;
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        let _ = f.read(&mut buf); // discard; EWOULDBLOCK is fine
        if stop_when_up && daemon_alive(sock) {
            // Give it another moment to finish creating the session.
            std::thread::sleep(Duration::from_millis(400));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
