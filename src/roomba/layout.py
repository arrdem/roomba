"""Discovering Bazel's on-disk layout under an output_user_root.

Bazel keys one output base per workspace directory, named ``md5(workspace_path)``,
all as siblings under the output_user_root (``~/.cache/bazel/_bazel_$USER``).

The md5 is one-way, but we never need to invert it: Bazel drops the workspace path
verbatim into ``<output_base>/DO_NOT_BUILD_HERE``. That file is the reverse mapping,
which is what makes orphan detection exact rather than a guess.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

# Output bases are md5 hex digests. The output_user_root also holds `cache/` (the
# repository cache) and `install/` (unpacked bazel binaries) as siblings -- matching
# on this shape is what keeps the reaper from eating them.
OUTPUT_BASE_RE = re.compile(r"^[0-9a-f]{32}$")

# Cheap freshness markers, in preference order. Walking a whole output base to find
# the newest mtime costs a recursive stat over ~100k files times ~600 bases; these
# few paths give the same answer for a couple of stats each. command.profile.gz is
# rewritten by every bazel command, so it is the truest "last used" signal.
FRESHNESS_MARKERS = (
    "command.profile.gz",
    "server/server.starttime",
    "action_cache",
    "server",
)


def default_output_user_root() -> Path:
    """Where bazel keeps output bases, absent an explicit --output_user_root."""
    return Path(
        os.environ.get("TEST_TMPDIR")
        or Path.home() / ".cache" / "bazel" / f"_bazel_{os.environ.get('USER') or Path.home().name}"
    )


@dataclass(frozen=True)
class OutputBase:
    """One ``<output_user_root>/<md5>`` directory."""

    path: Path
    workspace: Path | None
    """The workspace this base was built for, per DO_NOT_BUILD_HERE. None if unreadable."""

    last_used: float
    """Unix mtime of the newest freshness marker."""

    @property
    def name(self) -> str:
        return self.path.name

    @property
    def orphaned(self) -> bool:
        """True when the workspace this base serves is gone.

        An unreadable DO_NOT_BUILD_HERE is deliberately NOT an orphan: a base too
        broken to say who it belongs to is left for the age rule to collect, rather
        than deleted on the strength of a missing file.
        """
        return self.workspace is not None and not self.workspace.exists()


def read_workspace(base: Path) -> Path | None:
    """Read the workspace path a base was created for, or None if unavailable."""
    try:
        raw = (base / "DO_NOT_BUILD_HERE").read_text().strip()
    except OSError:
        return None
    return Path(raw) if raw else None


def last_used(base: Path) -> float:
    """Newest mtime among the freshness markers, falling back to the base itself."""
    newest = 0.0
    for marker in FRESHNESS_MARKERS:
        try:
            newest = max(newest, (base / marker).stat().st_mtime)
        except OSError:
            continue
    if newest:
        return newest
    try:
        return base.stat().st_mtime
    except OSError:
        return 0.0


def iter_output_bases(output_user_root: Path):
    """Yield every output base under `output_user_root`.

    Only md5-shaped directories are considered, so `cache/` and `install/` are
    structurally excluded rather than special-cased.
    """
    try:
        entries = sorted(output_user_root.iterdir())
    except OSError:
        return
    for entry in entries:
        if not OUTPUT_BASE_RE.match(entry.name) or not entry.is_dir():
            continue
        yield OutputBase(
            path=entry,
            workspace=read_workspace(entry),
            last_used=last_used(entry),
        )
