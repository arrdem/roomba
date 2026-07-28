//! Discovering Bazel's on-disk layout under an output_user_root.
//!
//! Bazel keys one output base per workspace directory, named `md5(workspace_path)`,
//! all as siblings under the output_user_root (`~/.cache/bazel/_bazel_$USER`).
//!
//! The md5 is one-way, but we never need to invert it: Bazel drops the workspace path
//! verbatim into `<output_base>/DO_NOT_BUILD_HERE`. That file is the reverse mapping,
//! which is what makes orphan detection exact rather than a guess.

use std::fs;
use std::path::{Path, PathBuf};

/// Cheap freshness markers, in preference order. Walking a whole output base to find
/// the newest mtime costs a recursive stat over ~100k files times ~600 bases; these
/// few paths give the same answer for a couple of stats each. `command.profile.gz` is
/// rewritten by every bazel command, so it is the truest "last used" signal.
const FRESHNESS_MARKERS: [&str; 4] = [
    "command.profile.gz",
    "server/server.starttime",
    "action_cache",
    "server",
];

/// Whether `name` is an md5 hex digest: exactly 32 lowercase hex characters.
///
/// Output bases are md5 hex digests. The output_user_root also holds `cache/` (the
/// repository cache) and `install/` (unpacked bazel binaries) as siblings -- matching
/// on this shape is what keeps the reaper from eating them.
fn is_output_base_name(name: &str) -> bool {
    name.len() == 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where bazel keeps output bases, absent an explicit `--output_user_root`.
pub fn default_output_user_root() -> PathBuf {
    if let Ok(t) = std::env::var("TEST_TMPDIR") {
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    let home = home_dir();
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            home.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        });
    home.join(".cache")
        .join("bazel")
        .join(format!("_bazel_{user}"))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One `<output_user_root>/<md5>` directory.
#[derive(Debug, Clone)]
pub struct OutputBase {
    pub path: PathBuf,
    /// The workspace this base was built for, per DO_NOT_BUILD_HERE. None if unreadable.
    pub workspace: Option<PathBuf>,
    /// Unix mtime of the newest freshness marker.
    pub last_used: f64,
}

impl OutputBase {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }

    /// True when the workspace this base serves is gone.
    ///
    /// An unreadable DO_NOT_BUILD_HERE is deliberately NOT an orphan: a base too
    /// broken to say who it belongs to is left for the age rule to collect, rather
    /// than deleted on the strength of a missing file.
    pub fn orphaned(&self) -> bool {
        match &self.workspace {
            Some(ws) => !ws.exists(),
            None => false,
        }
    }
}

/// Read the workspace path a base was created for, or None if unavailable.
pub fn read_workspace(base: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(base.join("DO_NOT_BUILD_HERE")).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else {
        Some(PathBuf::from(raw))
    }
}

/// Newest mtime among the freshness markers, falling back to the base itself.
pub fn last_used(base: &Path) -> f64 {
    let mut newest = 0.0_f64;
    for marker in FRESHNESS_MARKERS {
        if let Some(m) = mtime(&base.join(marker)) {
            if m > newest {
                newest = m;
            }
        }
    }
    if newest != 0.0 {
        return newest;
    }
    mtime(base).unwrap_or(0.0)
}

/// Unix mtime of `path` as float seconds (following symlinks), or None on error.
fn mtime(path: &Path) -> Option<f64> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::metadata(path).ok()?;
    Some(md.mtime() as f64 + md.mtime_nsec() as f64 / 1e9)
}

/// Every output base under `output_user_root`, sorted by name.
///
/// Only md5-shaped directories are considered, so `cache/` and `install/` are
/// structurally excluded rather than special-cased.
pub fn iter_output_bases(output_user_root: &Path) -> Vec<OutputBase> {
    let mut entries: Vec<PathBuf> = match fs::read_dir(output_user_root) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();

    let mut out = Vec::new();
    for entry in entries {
        let name = match entry.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_output_base_name(&name) || !entry.is_dir() {
            continue;
        }
        out.push(OutputBase {
            workspace: read_workspace(&entry),
            last_used: last_used(&entry),
            path: entry,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    //! The census: what counts as an output base, and who does it belong to.
    use super::*;
    use crate::testutil::{age, make_base, mtime_secs, output_user_root, TmpDir};

    const MD5_A: &str = "00000000000000000000000000000000";
    const MD5_B: &str = "11111111111111111111111111111111";

    #[test]
    fn reads_workspace_from_do_not_build_here() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let base = make_base(&root, MD5_A, Some(&ws), 0.0, true);
        assert_eq!(read_workspace(&base), Some(ws));
    }

    #[test]
    fn workspace_is_none_when_marker_missing() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let base = root.join(MD5_A);
        std::fs::create_dir(&base).unwrap();
        assert_eq!(read_workspace(&base), None);
    }

    #[test]
    fn iter_skips_cache_and_install_siblings() {
        // The repository cache and install base live beside the output bases; sweeping
        // either would be a disaster, so they must never be enumerated.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        make_base(&root, MD5_A, Some(&ws), 0.0, true);
        std::fs::create_dir(root.join("cache")).unwrap();
        std::fs::create_dir(root.join("install")).unwrap();
        std::fs::create_dir(root.join("not-an-md5")).unwrap();
        std::fs::create_dir(root.join("f".repeat(31))).unwrap(); // too short to be an md5

        let names: Vec<String> = iter_output_bases(&root).iter().map(|b| b.name()).collect();
        assert_eq!(names, vec![MD5_A.to_string()]);
    }

    #[test]
    fn orphaned_tracks_workspace_existence() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        make_base(&root, MD5_A, Some(&ws), 0.0, true);
        make_base(&root, MD5_B, Some(&tmp.join("vanished")), 0.0, true);

        let found: std::collections::HashMap<String, OutputBase> = iter_output_bases(&root)
            .into_iter()
            .map(|b| (b.name(), b))
            .collect();
        assert!(!found[MD5_A].orphaned());
        assert!(found[MD5_B].orphaned());
    }

    #[test]
    fn unreadable_workspace_is_not_an_orphan() {
        // A base too broken to name its workspace is left to the age rule, not swept.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        std::fs::create_dir(root.join(MD5_A)).unwrap();
        let found = iter_output_bases(&root);
        assert_eq!(found[0].workspace, None);
        assert!(!found[0].orphaned());
    }

    #[test]
    fn last_used_prefers_newest_marker() {
        // A cold base with one recently-touched marker counts as recently used.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let base = make_base(&root, MD5_A, Some(&ws), 100.0, true);
        let server = base.join("server");
        std::fs::create_dir(&server).unwrap();
        std::fs::write(server.join("server.starttime"), "x").unwrap();
        age(&server, 100.0);
        let starttime = age(&server.join("server.starttime"), 1.0);

        assert_eq!(last_used(&base), mtime_secs(&starttime));
    }
}
