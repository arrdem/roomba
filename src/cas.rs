//! LRU sweep of Bazel's repository cache CAS.
//!
//! The repository cache (`<output_user_root>/cache/repos/v1/content_addressable/<algo>/
//! <hash>/<file>`) holds every external archive bazel has ever downloaded, forever --
//! it has no eviction of its own. Everything in it is by definition re-fetchable, so
//! evicting a hot entry costs one download over the lab link, not a rebuild.
//!
//! atime is the right signal here (an entry is *read* on cache hit, not written), with
//! one caveat worth knowing: these filesystems mount `relatime`, which only refreshes
//! atime when the existing one is over 24h stale. So an atime can lag real access by up
//! to 24h, and a 48h cutoff can evict something last touched ~24h ago. That is a
//! re-download, not a correctness problem -- but it is why the cutoff should stay
//! comfortably above 24h rather than being tuned down to, say, 6h, where relatime would
//! make it meaningless.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::util::now;

/// The module's own default cutoff, mirroring the Python constant. The CLI drives the
/// CAS sweep with the shared `--max-age-hours` value, so this stands as the documented
/// default for direct callers of [`stale`].
#[allow(dead_code)]
pub const DEFAULT_MAX_AGE_HOURS: f64 = 48.0;

/// The content-addressable store under a repository cache.
pub fn cas_root(output_user_root: &Path) -> PathBuf {
    output_user_root
        .join("cache")
        .join("repos")
        .join("v1")
        .join("content_addressable")
}

/// One cached file in the CAS.
#[derive(Debug, Clone)]
pub struct CasEntry {
    pub path: PathBuf,
    pub atime: f64,
    /// Apparent size in bytes. Carried on the record for parity with the Python
    /// dataclass; space actually reclaimed is measured from st_blocks in the reaper.
    #[allow(dead_code)]
    pub size: u64,
    pub nlink: u64,
}

impl CasEntry {
    /// Whether this file is hardlinked elsewhere.
    ///
    /// Defensive, not load-bearing: bazel *copies* cache entries into output bases by
    /// default, and measured here every one of 8964 CAS files had nlink=1. It only
    /// becomes real under --experimental_repository_cache_hardlinks, where unlinking
    /// an entry would free nothing until the base holding the other link goes too.
    /// The check is free (we have already stat'd), so we keep it rather than claim
    /// space we did not reclaim if that flag ever gets turned on.
    pub fn shared(&self) -> bool {
        self.nlink > 1
    }
}

/// Every file in the CAS.
pub fn iter_entries(root: &Path) -> Vec<CasEntry> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    collect(root, &mut out);
    out
}

/// Recursively gather CAS files, mirroring Python's `rglob("*")` + `path.stat()`:
/// recurse only into *real* directories (never following a symlinked dir, exactly as
/// `rglob(recurse_symlinks=False)` does), and for every other entry stat with symlinks
/// followed, keeping the ones that resolve to a regular file.
fn collect(dir: &Path, out: &mut Vec<CasEntry>) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut children: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort();
    for path in children {
        // symlink_metadata never follows, so is_dir() here is true only for a genuine
        // directory. A symlinked directory falls through to the stat-follow branch,
        // where is_file() is false -- so it is neither counted nor descended into,
        // matching rglob, which yields it but does not recurse through it.
        if fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
        {
            collect(&path, out);
            continue;
        }
        // stat() follows symlinks, matching Python's Path.stat()/is_file(); a broken
        // symlink errors here and is skipped, as rglob's stat would raise and continue.
        let st = match fs::metadata(&path) {
            Ok(st) => st,
            Err(_) => continue,
        };
        if st.is_file() {
            out.push(CasEntry {
                atime: st.atime() as f64 + st.atime_nsec() as f64 / 1e9,
                size: st.size(),
                nlink: st.nlink(),
                path,
            });
        }
    }
}

/// The entries not read within `max_age_hours`.
pub fn stale(entries: Vec<CasEntry>, max_age_hours: f64) -> Vec<CasEntry> {
    let cutoff = now() - max_age_hours * 3600.0;
    entries.into_iter().filter(|e| e.atime < cutoff).collect()
}

#[cfg(test)]
mod tests {
    //! Repository cache eviction by atime.
    use super::*;
    use crate::testutil::{set_times, TmpDir};

    fn entry(root: &Path, name: &str, age_hours: f64, content: &[u8]) -> PathBuf {
        let d = root.join("sha256").join(name);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("file");
        std::fs::write(&f, content).unwrap();
        set_times(&f, now() - age_hours * 3600.0);
        f
    }

    #[test]
    fn cas_root_is_under_the_repository_cache() {
        let tmp = TmpDir::new();
        assert_eq!(
            cas_root(&tmp.path),
            tmp.path
                .join("cache")
                .join("repos")
                .join("v1")
                .join("content_addressable")
        );
    }

    #[test]
    fn missing_cas_yields_nothing() {
        let tmp = TmpDir::new();
        assert!(iter_entries(&cas_root(&tmp.path)).is_empty());
    }

    #[test]
    fn stale_entries_are_selected_by_atime() {
        let tmp = TmpDir::new();
        let root = cas_root(&tmp.path);
        entry(&root, "cold", 100.0, b"payload");
        entry(&root, "warm", 1.0, b"payload");

        let doomed = stale(iter_entries(&root), DEFAULT_MAX_AGE_HOURS);
        let names: Vec<String> = doomed
            .iter()
            .map(|e| {
                e.path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["cold".to_string()]);
    }

    #[test]
    fn cutoff_is_configurable() {
        let tmp = TmpDir::new();
        let root = cas_root(&tmp.path);
        entry(&root, "a", 10.0, b"payload");
        assert!(stale(iter_entries(&root), DEFAULT_MAX_AGE_HOURS).is_empty());
        assert_eq!(stale(iter_entries(&root), 6.0).len(), 1);
    }

    #[test]
    fn hardlinked_entries_are_flagged_as_shared() {
        // Unlinking a hardlinked entry frees nothing until the other link goes too.
        let tmp = TmpDir::new();
        let root = cas_root(&tmp.path);
        let f = entry(&root, "linked", 100.0, b"payload");
        std::fs::create_dir(tmp.join("elsewhere")).unwrap();
        std::fs::hard_link(&f, tmp.join("elsewhere").join("held")).unwrap();

        let e = iter_entries(&root)
            .into_iter()
            .find(|e| e.path == f)
            .unwrap();
        assert!(e.shared());
        assert_eq!(e.size, 7); // b"payload"
    }

    #[test]
    fn unshared_entry_is_not_flagged() {
        let tmp = TmpDir::new();
        let root = cas_root(&tmp.path);
        entry(&root, "solo", 100.0, b"payload");
        assert!(!iter_entries(&root)[0].shared());
    }

    #[test]
    fn symlinked_dir_is_not_descended_into() {
        // rglob(recurse_symlinks=False) never walks *through* a symlinked directory, so
        // enumeration must not reach files outside the CAS -- otherwise a stray symlink
        // would hand their paths to sweep_file and unlink real files elsewhere.
        use std::os::unix::fs::symlink;
        let tmp = TmpDir::new();
        let outside = tmp.join("precious");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"data").unwrap();

        let root = cas_root(&tmp.path);
        std::fs::create_dir_all(root.join("sha256")).unwrap();
        symlink(&outside, root.join("sha256").join("link")).unwrap();

        let paths: Vec<_> = iter_entries(&root).into_iter().map(|e| e.path).collect();
        assert!(
            paths.is_empty(),
            "symlinked dir was followed, reached: {paths:?}"
        );
        assert!(outside.join("keep").exists());
    }
}
