//! Deciding whether an output base is in use right now.
//!
//! Deleting an output base out from under a running bazel server corrupts the build it
//! is in the middle of, so liveness is a hard veto in `policy`, never a heuristic to be
//! weighed. Two independent signals, either of which vetoes:
//!
//!   * `<base>/lock` -- the bazel server holds an exclusive flock on this for its whole
//!     life. Probing it is the same mechanism bazel itself uses, so it stays correct
//!     even if a server dies without cleaning up its pid file.
//!   * `<base>/server/server.pid.txt` -- covers servers whose lock we can't open (e.g.
//!     EPERM), and gives us a pid to report.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// The pid recorded by the server owning `base`, if it is still running.
pub fn server_pid(base: &Path) -> Option<i32> {
    let raw = fs::read_to_string(base.join("server").join("server.pid.txt")).ok()?;
    let pid: i32 = raw.trim().parse().ok()?;
    if pid_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

/// Whether `pid` names a live process.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) probes for existence without delivering a signal.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    match errno() {
        // No such process.
        libc::ESRCH => false,
        // Someone else's process: alive, just not ours to signal.
        libc::EPERM => true,
        _ => true,
    }
}

/// Whether a bazel server holds `<base>/lock`.
///
/// We take the lock only to see whether we can, and drop it immediately. A missing
/// lock file means no server ever started here. Anything we can't interpret is
/// reported as held: an ambiguous answer must not authorize a delete.
pub fn lock_held(base: &Path) -> bool {
    let lock = base.join("lock");
    let file = match fs::OpenOptions::new().read(true).write(true).open(&lock) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        // `file` drops here, closing the fd, mirroring the explicit close in Python.
        false
    } else {
        let e = errno();
        e == libc::EACCES || e == libc::EAGAIN
    }
}

/// Whether anything is actively using `base`.
pub fn in_use(base: &Path) -> bool {
    lock_held(base) || server_pid(base).is_some()
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
