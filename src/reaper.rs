//! Removing what policy condemned, and counting what that gave back.
//!
//! Space is counted *during* the delete, never predicted before it. Measuring a tree up
//! front means walking every inode just to produce a number the delete would hand us for
//! free -- on ~500 output bases that is a full `du` of the whole cache, tens of minutes,
//! to tell us something we learn anyway a moment later. So a dry run reports *what* would
//! go and says nothing about bytes, and only `--apply` reports space, which it knows
//! exactly.
//!
//! The accounting leans on a property of unlink: a file with more than one link frees
//! nothing when you remove one of them. Counting `st_blocks` only when `st_nlink == 1`
//! is therefore exactly right, and needs no dedupe bookkeeping -- for a tree holding two
//! links to one inode, the first unlink counts nothing (nlink 2 -> 1) and the second
//! counts the blocks.
//!
//! This is defensive rather than load-bearing today: measured on this workstation,
//! nothing in the repository cache or an output base's external/ was hardlinked at all.
//! It costs nothing (the stat is needed to unlink anyway) and keeps the number honest if
//! that changes -- e.g. under --experimental_repository_cache_hardlinks.
//!
//! Deletion is idempotent, so ENOENT is success, not failure: the goal is that the thing
//! be gone, and it is gone. This matters because roomba is not the only thing sweeping --
//! a human with `sudo rm -rf`, or a second roomba, will race the walk and delete entries
//! between our scandir and our unlink. Treating that as an error produced spectacular
//! nonsense once: 38098 "failures" on a base that had, in fact, been successfully removed.
//! Only a real refusal (EACCES, EBUSY, ENOTEMPTY) counts against us.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const BLOCK: u64 = 512;

/// The outcome of removing one path.
#[derive(Debug, Clone)]
pub struct Removal {
    /// The path this outcome describes. Kept as part of the record (mirroring the
    /// Python dataclass) even though the CLI reports the base name instead.
    #[allow(dead_code)]
    pub path: PathBuf,
    /// Bytes actually handed back. Always 0 for a dry run, which does not measure.
    pub freed: u64,
    pub error: Option<String>,
}

impl Removal {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Add the owner-write bit to `directory` so its entries can be unlinked.
///
/// Bazel marks action output directories read-only (0555) to protect build outputs
/// from being edited. Unlink checks the *parent directory's* write bit, not the
/// file's, so without this every delete inside them fails EACCES, the directory is
/// left non-empty, and the rmdir cascade fails all the way up -- a base can have
/// ~1500 such directories. This is why `sudo rm -rf` on a base works where an
/// unprivileged delete does not: root bypasses the permission check. We are removing
/// the tree outright, so relaxing the mode on the way down costs nothing.
fn force_writable(directory: &Path) {
    if let Ok(md) = fs::symlink_metadata(directory) {
        let mode = md.permissions().mode();
        if mode & libc::S_IWUSR == 0 {
            let _ =
                fs::set_permissions(directory, fs::Permissions::from_mode(mode | libc::S_IWUSR));
        }
    }
}

/// Space an unlink of this inode will actually release.
fn freed_by_unlink(md: &fs::Metadata) -> u64 {
    if md.nlink() == 1 {
        md.blocks() * BLOCK
    } else {
        0
    }
}

/// Remove a directory tree, counting space as each inode goes.
///
/// Deletes bottom-up so directories are empty by the time we rmdir them. A file we
/// cannot stat or unlink is skipped rather than aborting the sweep: one unreadable
/// inode in one base should not strand the other 500.
pub fn sweep_tree(path: &Path, dry_run: bool) -> Removal {
    if dry_run {
        return Removal {
            path: path.to_path_buf(),
            freed: 0,
            error: None,
        };
    }

    let mut freed = 0u64;
    let mut failures = 0u64;
    // Post-order: empty each directory's contents before we rmdir it.
    walk_postorder(path, &mut freed, &mut failures);

    match fs::remove_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            return Removal {
                path: path.to_path_buf(),
                freed,
                error: Some(e.to_string()),
            }
        }
    }
    Removal {
        path: path.to_path_buf(),
        freed,
        error: (failures > 0).then(|| format!("{failures} entries could not be removed")),
    }
}

/// Mirror `os.walk(dir, topdown=False, followlinks=False)` for one directory: recurse
/// into real subdirectories first, then relax this directory's mode, unlink its files,
/// and remove its (now empty) subdirectories.
fn walk_postorder(dir: &Path, freed: &mut u64, failures: &mut u64) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        // os.walk swallows scandir errors (onerror=lambda _: None).
        Err(_) => return,
    };

    // Classify like os.walk: is_dir() follows symlinks, so a symlink-to-dir lands in
    // `subdirs` (and is later rmdir'd -> unlinked), while a symlink-to-file and any
    // broken symlink land in `files`.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for ent in rd {
        let p = match ent {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        let is_dir = fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            subdirs.push(p);
        } else {
            files.push(p);
        }
    }

    // Recurse only into true directories, never following a symlink.
    for sd in &subdirs {
        let is_symlink = fs::symlink_metadata(sd)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_symlink {
            walk_postorder(sd, freed, failures);
        }
    }

    // Before touching anything in this directory -- both its files and, since a child
    // rmdir writes to its parent, its subdirectories.
    force_writable(dir);

    for f in &files {
        // lstat then unlink, both symlink-aware, so a symlink's own (tiny) blocks are
        // what we count and its target is left untouched.
        let md = match fs::symlink_metadata(f) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(_) => {
                *failures += 1;
                continue;
            }
        };
        match fs::remove_file(f) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(_) => {
                *failures += 1;
                continue;
            }
        }
        *freed += freed_by_unlink(&md);
    }

    for sd in &subdirs {
        match fs::remove_dir(sd) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => {
                // A symlink to a directory, or a dir we failed to empty above.
                match fs::remove_file(sd) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(_) => *failures += 1,
                }
            }
        }
    }
}

/// Unlink a single file, counting the space it actually released.
pub fn sweep_file(path: &Path, dry_run: bool) -> Removal {
    if dry_run {
        return Removal {
            path: path.to_path_buf(),
            freed: 0,
            error: None,
        };
    }
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Removal {
                path: path.to_path_buf(),
                freed: 0,
                error: None,
            }
        }
        Err(e) => {
            return Removal {
                path: path.to_path_buf(),
                freed: 0,
                error: Some(e.to_string()),
            }
        }
    };
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            // Same read-only-parent problem as sweep_tree; retry once having relaxed it.
            if let Some(parent) = path.parent() {
                force_writable(parent);
            }
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    return Removal {
                        path: path.to_path_buf(),
                        freed: 0,
                        error: Some(e.to_string()),
                    }
                }
            }
        }
        Err(e) => {
            return Removal {
                path: path.to_path_buf(),
                freed: 0,
                error: Some(e.to_string()),
            }
        }
    }
    Removal {
        path: path.to_path_buf(),
        freed: freed_by_unlink(&md),
        error: None,
    }
}

/// Render a byte count the way df does.
pub fn human(mut size: f64) -> String {
    for unit in ["B", "K", "M", "G", "T"] {
        if size.abs() < 1024.0 || unit == "T" {
            return if unit == "B" {
                format!("{size:.0}{unit}")
            } else {
                format!("{size:.1}{unit}")
            };
        }
        size /= 1024.0;
    }
    format!("{size:.1}T")
}

#[cfg(test)]
mod tests {
    //! Removal, and the space accounting that rides along with it.
    use super::*;
    use crate::testutil::TmpDir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn chmod(p: &Path, mode: u32) {
        std::fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn dry_run_deletes_nothing_and_measures_nothing() {
        // The property everything else rests on.
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("payload"), vec![b'x'; 8192]).unwrap();

        let r = sweep_tree(&tree, true);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
        assert!(tree.exists());
    }

    #[test]
    fn apply_deletes_and_counts() {
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        std::fs::create_dir_all(tree.join("nested")).unwrap();
        std::fs::write(tree.join("nested").join("payload"), vec![b'x'; 65536]).unwrap();

        let r = sweep_tree(&tree, false);
        assert!(r.ok());
        assert!(r.freed >= 65536);
        assert!(!tree.exists());
    }

    #[test]
    fn hardlinked_inode_is_counted_once() {
        // Two links to one inode give back one inode's worth, on the last unlink.
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        std::fs::create_dir(&tree).unwrap();
        let f = tree.join("a");
        std::fs::write(&f, vec![b'x'; 65536]).unwrap();
        std::fs::hard_link(&f, tree.join("b")).unwrap();

        let r = sweep_tree(&tree, false);
        assert!(r.ok());
        assert!(65536 <= r.freed && r.freed < 65536 * 2);
    }

    #[test]
    fn link_held_outside_the_tree_frees_nothing() {
        // A CAS entry hardlinked into a live output base must not be claimed as reclaimed.
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        std::fs::create_dir(&tree).unwrap();
        let f = tree.join("entry");
        std::fs::write(&f, vec![b'x'; 65536]).unwrap();
        std::fs::create_dir(tmp.join("elsewhere")).unwrap();
        std::fs::hard_link(&f, tmp.join("elsewhere").join("held")).unwrap();

        let r = sweep_tree(&tree, false);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
        assert!(tmp.join("elsewhere").join("held").exists());
    }

    #[test]
    fn symlinks_are_removed_not_followed() {
        // A base full of bazel's convenience symlinks must not sweep their targets.
        let tmp = TmpDir::new();
        let outside = tmp.join("precious");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"data").unwrap();

        let tree = tmp.join("base");
        std::fs::create_dir(&tree).unwrap();
        symlink(&outside, tree.join("bazel-out")).unwrap();

        let r = sweep_tree(&tree, false);
        assert!(r.ok());
        assert!(!tree.exists());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn read_only_directory_is_swept() {
        // Bazel marks action output dirs 0555; unlink checks the PARENT's write bit.
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        let locked = tree.join("bazel-out").join("bin").join("install");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("payload"), vec![b'x'; 8192]).unwrap();
        chmod(&locked.join("payload"), 0o555);
        chmod(&locked, 0o555);

        let r = sweep_tree(&tree, false);
        assert!(r.ok(), "{:?}", r.error);
        assert!(!tree.exists());
        assert!(r.freed >= 8192);
    }

    #[test]
    fn read_only_nested_directories_are_swept() {
        // A read-only dir inside a read-only dir: the rmdir also needs the parent's bit.
        let tmp = TmpDir::new();
        let tree = tmp.join("base");
        let inner = tree.join("outer").join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("f"), b"data").unwrap();
        chmod(&inner, 0o555);
        chmod(&tree.join("outer"), 0o555);

        let r = sweep_tree(&tree, false);
        assert!(r.ok(), "{:?}", r.error);
        assert!(!tree.exists());
    }

    #[test]
    fn read_only_cas_entry_is_unlinked() {
        // Repository cache entries sit in read-only directories too.
        let tmp = TmpDir::new();
        let d = tmp.join("sha256").join("abc");
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("file");
        std::fs::write(&f, vec![b'x'; 8192]).unwrap();
        chmod(&f, 0o444);
        chmod(&d, 0o555);

        let r = sweep_file(&f, false);
        assert!(r.ok(), "{:?}", r.error);
        assert!(!f.exists());
    }

    #[test]
    fn sweeping_an_absent_tree_is_success() {
        // Idempotent: the goal is that it be gone, and it is gone.
        let tmp = TmpDir::new();
        let r = sweep_tree(&tmp.join("absent"), false);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
    }

    #[test]
    fn sweeping_an_absent_file_is_success() {
        // The file-level analogue: a CAS entry another sweeper removed first. Python
        // proved this by monkeypatching os.unlink to raise ENOENT mid-call; the same
        // NotFound-is-success contract is what this and the absent-tree case cover.
        let tmp = TmpDir::new();
        let r = sweep_file(&tmp.join("absent"), false);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
    }

    #[test]
    fn file_dry_run_keeps_file() {
        let tmp = TmpDir::new();
        let f = tmp.join("entry");
        std::fs::write(&f, b"data").unwrap();
        let r = sweep_file(&f, true);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
        assert!(f.exists());
    }

    #[test]
    fn file_apply_unlinks_and_counts() {
        let tmp = TmpDir::new();
        let f = tmp.join("entry");
        std::fs::write(&f, vec![b'x'; 8192]).unwrap();
        let r = sweep_file(&f, false);
        assert!(r.ok());
        assert!(r.freed >= 8192);
        assert!(!f.exists());
    }

    #[test]
    fn file_with_another_link_frees_nothing() {
        let tmp = TmpDir::new();
        let f = tmp.join("entry");
        std::fs::write(&f, vec![b'x'; 8192]).unwrap();
        std::fs::hard_link(&f, tmp.join("other")).unwrap();
        let r = sweep_file(&f, false);
        assert!(r.ok());
        assert_eq!(r.freed, 0);
    }

    #[test]
    fn human_renders_like_df() {
        assert_eq!(human(512.0), "512B");
        assert_eq!(human(1536.0), "1.5K");
        assert_eq!(human(5.0 * 1024f64.powi(3)), "5.0G");
    }
}
