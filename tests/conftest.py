import os
import time
from pathlib import Path

import pytest


@pytest.fixture
def age():
    """Backdate a path's mtime/atime by `hours`.

    Needed because every freshness marker roomba consults is, by construction, a
    file the test just created -- so a test that wants an old base has to age each
    marker it adds, not just the one make_base wrote.
    """

    def _age(path: Path, hours: float) -> Path:
        when = time.time() - hours * 3600.0
        os.utime(path, (when, when))
        return path

    return _age


@pytest.fixture
def output_user_root(tmp_path: Path) -> Path:
    root = tmp_path / "_bazel_test"
    root.mkdir()
    return root


@pytest.fixture
def make_base(output_user_root: Path):
    """Build a realistic output base: an md5-named dir pointing at a workspace."""

    def _make(name: str, workspace: Path | None, age_hours: float = 0.0, lock: bool = True) -> Path:
        base = output_user_root / name
        base.mkdir()
        if workspace is not None:
            (base / "DO_NOT_BUILD_HERE").write_text(f"{workspace}\n")
        marker = base / "command.profile.gz"
        marker.write_bytes(b"")
        if lock:
            (base / "lock").write_bytes(b"")
        when = time.time() - age_hours * 3600.0
        for p in (marker, base):
            os.utime(p, (when, when))
        return base

    return _make
