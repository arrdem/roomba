//! Test-only helpers, standing in for the pytest fixtures (`tmp_path`, `age`,
//! `output_user_root`, `make_base`) the Python suite leaned on.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A scratch directory that removes itself (best-effort) on drop -- the `tmp_path`
/// fixture equivalent.
pub struct TmpDir {
    pub path: PathBuf,
}

impl TmpDir {
    pub fn new() -> TmpDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("roomba-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).expect("create tmpdir");
        TmpDir { path }
    }

    pub fn join(&self, p: &str) -> PathBuf {
        self.path.join(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        force_writable_tree(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn force_writable_tree(dir: &Path) {
    if let Ok(md) = std::fs::symlink_metadata(dir) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        if md.file_type().is_dir() {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    force_writable_tree(&e.path());
                }
            }
        }
    }
}

/// Set a path's atime and mtime to `secs` since the epoch (the `os.utime` primitive).
pub fn set_times(path: &Path, secs: f64) {
    let tv = libc::timeval {
        tv_sec: secs.trunc() as libc::time_t,
        tv_usec: (secs.fract() * 1e6).round() as libc::suseconds_t,
    };
    let times = [tv, tv];
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed on {}", path.display());
}

/// Backdate a path's atime/mtime by `hours`, returning it (the `age` fixture).
pub fn age(path: &Path, hours: f64) -> PathBuf {
    set_times(path, crate::util::now() - hours * 3600.0);
    path.to_path_buf()
}

/// Read a path's mtime the same way `layout::last_used` does, for exact comparison.
pub fn mtime_secs(path: &Path) -> f64 {
    let md = std::fs::metadata(path).unwrap();
    md.mtime() as f64 + md.mtime_nsec() as f64 / 1e9
}

/// A fresh output_user_root directory (the `output_user_root` fixture).
pub fn output_user_root(tmp: &TmpDir) -> PathBuf {
    let root = tmp.join("_bazel_test");
    std::fs::create_dir(&root).unwrap();
    root
}

/// Build a realistic output base: an md5-named dir pointing at a workspace (the
/// `make_base` fixture). Ages both the freshness marker and the base directory.
pub fn make_base(
    root: &Path,
    name: &str,
    workspace: Option<&Path>,
    age_hours: f64,
    lock: bool,
) -> PathBuf {
    let base = root.join(name);
    std::fs::create_dir(&base).unwrap();
    if let Some(ws) = workspace {
        std::fs::write(
            base.join("DO_NOT_BUILD_HERE"),
            format!("{}\n", ws.display()),
        )
        .unwrap();
    }
    let marker = base.join("command.profile.gz");
    std::fs::write(&marker, b"").unwrap();
    if lock {
        std::fs::write(base.join("lock"), b"").unwrap();
    }
    let when = crate::util::now() - age_hours * 3600.0;
    set_times(&marker, when);
    set_times(&base, when);
    base
}
