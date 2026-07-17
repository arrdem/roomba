"""Removal, and the space accounting that rides along with it."""

import os
from unittest import mock

from roomba.reaper import human, sweep_file, sweep_tree


def test_dry_run_deletes_nothing_and_measures_nothing(tmp_path):
    """The property everything else rests on."""
    tree = tmp_path / "base"
    tree.mkdir()
    (tree / "payload").write_bytes(b"x" * 8192)

    r = sweep_tree(tree, dry_run=True)
    assert r.ok
    assert r.freed == 0
    assert tree.exists()


def test_apply_deletes_and_counts(tmp_path):
    tree = tmp_path / "base"
    (tree / "nested").mkdir(parents=True)
    (tree / "nested" / "payload").write_bytes(b"x" * 65536)

    r = sweep_tree(tree, dry_run=False)
    assert r.ok
    assert r.freed >= 65536
    assert not tree.exists()


def test_hardlinked_inode_is_counted_once(tmp_path):
    """Two links to one inode give back one inode's worth, on the last unlink."""
    tree = tmp_path / "base"
    tree.mkdir()
    f = tree / "a"
    f.write_bytes(b"x" * 65536)
    os.link(f, tree / "b")

    r = sweep_tree(tree, dry_run=False)
    assert r.ok
    assert 65536 <= r.freed < 65536 * 2


def test_link_held_outside_the_tree_frees_nothing(tmp_path):
    """A CAS entry hardlinked into a live output base must not be claimed as reclaimed."""
    tree = tmp_path / "base"
    tree.mkdir()
    f = tree / "entry"
    f.write_bytes(b"x" * 65536)
    (tmp_path / "elsewhere").mkdir()
    os.link(f, tmp_path / "elsewhere" / "held")

    r = sweep_tree(tree, dry_run=False)
    assert r.ok
    assert r.freed == 0
    assert (tmp_path / "elsewhere" / "held").exists()


def test_symlinks_are_removed_not_followed(tmp_path):
    """A base full of bazel's convenience symlinks must not sweep their targets."""
    outside = tmp_path / "precious"
    outside.mkdir()
    (outside / "keep").write_bytes(b"data")

    tree = tmp_path / "base"
    tree.mkdir()
    os.symlink(outside, tree / "bazel-out")

    r = sweep_tree(tree, dry_run=False)
    assert r.ok
    assert not tree.exists()
    assert (outside / "keep").exists()


def test_read_only_directory_is_swept(tmp_path):
    """Bazel marks action output dirs 0555; unlink checks the PARENT's write bit.

    Without forcing the mode, every delete inside such a directory fails EACCES and
    the whole base survives as a skeleton. This is the shape of a real output base,
    which can hold ~1500 read-only directories.
    """
    tree = tmp_path / "base"
    locked = tree / "bazel-out" / "bin" / "install"
    locked.mkdir(parents=True)
    (locked / "payload").write_bytes(b"x" * 8192)
    (locked / "payload").chmod(0o555)
    locked.chmod(0o555)

    r = sweep_tree(tree, dry_run=False)
    assert r.ok, r.error
    assert not tree.exists()
    assert r.freed >= 8192


def test_read_only_nested_directories_are_swept(tmp_path):
    """A read-only dir inside a read-only dir: the rmdir also needs the parent's bit."""
    tree = tmp_path / "base"
    inner = tree / "outer" / "inner"
    inner.mkdir(parents=True)
    (inner / "f").write_bytes(b"data")
    inner.chmod(0o555)
    (tree / "outer").chmod(0o555)

    r = sweep_tree(tree, dry_run=False)
    assert r.ok, r.error
    assert not tree.exists()


def test_read_only_cas_entry_is_unlinked(tmp_path):
    """Repository cache entries sit in read-only directories too."""
    d = tmp_path / "sha256" / "abc"
    d.mkdir(parents=True)
    f = d / "file"
    f.write_bytes(b"x" * 8192)
    f.chmod(0o444)
    d.chmod(0o555)

    r = sweep_file(f, dry_run=False)
    assert r.ok, r.error
    assert not f.exists()


def test_tree_vanishing_mid_sweep_is_not_a_failure(tmp_path):
    """Someone else deleting it first is success, not error -- roomba isn't alone.

    A human running `sudo rm -rf`, or a second roomba, races the walk and removes
    entries between our scandir and our unlink. Counting that as failure once
    reported 38098 "failures" on a base that had in fact been removed.
    """
    tree = tmp_path / "base"
    (tree / "sub").mkdir(parents=True)
    for i in range(3):
        (tree / "sub" / f"f{i}").write_bytes(b"x" * 4096)

    real_unlink = os.unlink
    raced = []

    def racing_unlink(p, *a, **kw):
        # On first contact, another sweeper takes the whole subtree. Use the real
        # unlink directly -- going through shutil here would re-enter this patch.
        if not raced and os.path.basename(p) == "f0":
            raced.append(True)
            sub = tree / "sub"
            for name in os.listdir(sub):
                real_unlink(os.path.join(sub, name))
            os.rmdir(sub)
            raise FileNotFoundError(2, "No such file or directory", str(p))
        return real_unlink(p, *a, **kw)

    with mock.patch("os.unlink", side_effect=racing_unlink):
        r = sweep_tree(tree, dry_run=False)

    assert raced, "the race never fired; the test proves nothing"
    assert r.ok, r.error
    assert not tree.exists()


def test_file_vanishing_before_unlink_is_not_a_failure(tmp_path):
    f = tmp_path / "entry"
    f.write_bytes(b"data")

    real_unlink = os.unlink

    def racing_unlink(p, *a, **kw):
        real_unlink(p, *a, **kw)
        raise FileNotFoundError(2, "No such file or directory", str(p))

    with mock.patch("os.unlink", side_effect=racing_unlink):
        r = sweep_file(f, dry_run=False)

    assert r.ok, r.error
    assert not f.exists()


def test_sweeping_an_absent_tree_is_success(tmp_path):
    """Idempotent: the goal is that it be gone, and it is gone."""
    r = sweep_tree(tmp_path / "absent", dry_run=False)
    assert r.ok
    assert r.freed == 0


def test_file_dry_run_keeps_file(tmp_path):
    f = tmp_path / "entry"
    f.write_bytes(b"data")

    r = sweep_file(f, dry_run=True)
    assert r.ok
    assert r.freed == 0
    assert f.exists()


def test_file_apply_unlinks_and_counts(tmp_path):
    f = tmp_path / "entry"
    f.write_bytes(b"x" * 8192)

    r = sweep_file(f, dry_run=False)
    assert r.ok
    assert r.freed >= 8192
    assert not f.exists()


def test_file_with_another_link_frees_nothing(tmp_path):
    f = tmp_path / "entry"
    f.write_bytes(b"x" * 8192)
    os.link(f, tmp_path / "other")

    r = sweep_file(f, dry_run=False)
    assert r.ok
    assert r.freed == 0


def test_human_renders_like_df():
    assert human(512) == "512B"
    assert human(1536) == "1.5K"
    assert human(5 * 1024**3) == "5.0G"
