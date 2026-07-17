"""Repository cache eviction by atime."""

import os
import time

from roomba.cas import cas_root, iter_entries, stale


def _entry(root, name, age_hours, content=b"payload"):
    d = root / "sha256" / name
    d.mkdir(parents=True)
    f = d / "file"
    f.write_bytes(content)
    when = time.time() - age_hours * 3600.0
    os.utime(f, (when, when))
    return f


def test_cas_root_is_under_the_repository_cache(tmp_path):
    assert cas_root(tmp_path) == tmp_path / "cache" / "repos" / "v1" / "content_addressable"


def test_missing_cas_yields_nothing(tmp_path):
    assert list(iter_entries(cas_root(tmp_path))) == []


def test_stale_entries_are_selected_by_atime(tmp_path):
    root = cas_root(tmp_path)
    _entry(root, "cold", age_hours=100)
    _entry(root, "warm", age_hours=1)

    doomed = stale(iter_entries(root))
    assert [e.path.parent.name for e in doomed] == ["cold"]


def test_cutoff_is_configurable(tmp_path):
    root = cas_root(tmp_path)
    _entry(root, "a", age_hours=10)
    assert stale(iter_entries(root)) == []
    assert len(stale(iter_entries(root), max_age_hours=6)) == 1


def test_hardlinked_entries_are_flagged_as_shared(tmp_path):
    """Unlinking a hardlinked entry frees nothing until the other link goes too."""
    root = cas_root(tmp_path)
    f = _entry(root, "linked", age_hours=100)
    (tmp_path / "elsewhere").mkdir()
    os.link(f, tmp_path / "elsewhere" / "held")

    entry = next(e for e in iter_entries(root) if e.path == f)
    assert entry.shared is True


def test_unshared_entry_is_not_flagged(tmp_path):
    root = cas_root(tmp_path)
    _entry(root, "solo", age_hours=100)
    assert next(iter_entries(root)).shared is False
