"""Removing what policy condemned, and counting what that gave back.

Space is counted *during* the delete, never predicted before it. Measuring a tree up
front means walking every inode just to produce a number the delete would hand us for
free -- on ~500 output bases that is a full `du` of the whole cache, tens of minutes,
to tell us something we learn anyway a moment later. So a dry run reports *what* would
go and says nothing about bytes, and only `--apply` reports space, which it knows
exactly.

The accounting leans on a property of unlink: a file with more than one link frees
nothing when you remove one of them. Counting `st_blocks` only when `st_nlink == 1`
is therefore exactly right, and needs no dedupe bookkeeping -- for a tree holding two
links to one inode, the first unlink counts nothing (nlink 2 -> 1) and the second
counts the blocks.

This is defensive rather than load-bearing today: measured on this workstation,
nothing in the repository cache or an output base's external/ was hardlinked at all.
It costs nothing (the stat is needed to unlink anyway) and keeps the number honest if
that changes -- e.g. under --experimental_repository_cache_hardlinks.

Deletion is idempotent, so ENOENT is success, not failure: the goal is that the thing
be gone, and it is gone. This matters because roomba is not the only thing sweeping --
a human with `sudo rm -rf`, or a second roomba, will race the walk and delete entries
between our scandir and our unlink. Treating that as an error produced spectacular
nonsense once: 38098 "failures" on a base that had, in fact, been successfully removed.
Only a real refusal (EACCES, EBUSY, ENOTEMPTY) counts against us.
"""

from __future__ import annotations

import os
import stat
from dataclasses import dataclass
from pathlib import Path

BLOCK = 512


def _force_writable(directory: str | Path) -> None:
    """Add the owner-write bit to `directory` so its entries can be unlinked.

    Bazel marks action output directories read-only (0555) to protect build outputs
    from being edited. Unlink checks the *parent directory's* write bit, not the
    file's, so without this every delete inside them fails EACCES, the directory is
    left non-empty, and the rmdir cascade fails all the way up -- a base can have
    ~1500 such directories. This is why `sudo rm -rf` on a base works where an
    unprivileged delete does not: root bypasses the permission check. We are removing
    the tree outright, so relaxing the mode on the way down costs nothing.
    """
    try:
        mode = os.lstat(directory).st_mode
        if not mode & stat.S_IWUSR:
            os.chmod(directory, mode | stat.S_IWUSR)
    except OSError:
        pass


@dataclass
class Removal:
    """The outcome of removing one path."""

    path: Path
    freed: int
    """Bytes actually handed back. Always 0 for a dry run, which does not measure."""

    error: str | None = None

    @property
    def ok(self) -> bool:
        return self.error is None


def _freed_by_unlink(st: os.stat_result) -> int:
    """Space an unlink of this inode will actually release."""
    return st.st_blocks * BLOCK if st.st_nlink == 1 else 0


def sweep_tree(path: Path, dry_run: bool = True) -> Removal:
    """Remove a directory tree, counting space as each inode goes.

    Deletes bottom-up so directories are empty by the time we rmdir them. A file we
    cannot stat or unlink is skipped rather than aborting the sweep: one unreadable
    inode in one base should not strand the other 500.
    """
    if dry_run:
        return Removal(path, 0)

    freed = 0
    failures = 0
    for root, dirs, files in os.walk(path, topdown=False, onerror=lambda _: None, followlinks=False):
        # Before touching anything in this directory -- both its files and, since a
        # child rmdir writes to its parent, its subdirectories.
        _force_writable(root)
        for name in files:
            p = os.path.join(root, name)
            try:
                st = os.lstat(p)
                os.unlink(p)
            except FileNotFoundError:
                continue
            except OSError:
                failures += 1
                continue
            freed += _freed_by_unlink(st)
        for name in dirs:
            d = os.path.join(root, name)
            try:
                os.rmdir(d)
            except FileNotFoundError:
                continue
            except OSError:
                # A symlink to a directory, or a dir we failed to empty above.
                try:
                    os.unlink(d)
                except FileNotFoundError:
                    continue
                except OSError:
                    failures += 1
    try:
        os.rmdir(path)
    except FileNotFoundError:
        pass
    except OSError as e:
        return Removal(path, freed, str(e))
    return Removal(path, freed, f"{failures} entries could not be removed" if failures else None)


def sweep_file(path: Path, dry_run: bool = True) -> Removal:
    """Unlink a single file, counting the space it actually released."""
    if dry_run:
        return Removal(path, 0)
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        return Removal(path, 0)
    except OSError as e:
        return Removal(path, 0, str(e))
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    except PermissionError:
        # Same read-only-parent problem as sweep_tree; retry once having relaxed it.
        _force_writable(path.parent)
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        except OSError as e:
            return Removal(path, 0, str(e))
    except OSError as e:
        return Removal(path, 0, str(e))
    return Removal(path, _freed_by_unlink(st))


def human(size: float) -> str:
    """Render a byte count the way df does."""
    for unit in ("B", "K", "M", "G", "T"):
        if abs(size) < 1024 or unit == "T":
            return f"{size:.0f}{unit}" if unit == "B" else f"{size:.1f}{unit}"
        size /= 1024
    return f"{size:.1f}T"
