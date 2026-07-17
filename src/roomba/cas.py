"""LRU sweep of Bazel's repository cache CAS.

The repository cache (`<output_user_root>/cache/repos/v1/content_addressable/<algo>/
<hash>/<file>`) holds every external archive bazel has ever downloaded, forever --
it has no eviction of its own. Everything in it is by definition re-fetchable, so
evicting a hot entry costs one download over the lab link, not a rebuild.

atime is the right signal here (an entry is *read* on cache hit, not written), with
one caveat worth knowing: these filesystems mount `relatime`, which only refreshes
atime when the existing one is over 24h stale. So an atime can lag real access by up
to 24h, and a 48h cutoff can evict something last touched ~24h ago. That is a
re-download, not a correctness problem -- but it is why the cutoff should stay
comfortably above 24h rather than being tuned down to, say, 6h, where relatime would
make it meaningless.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from pathlib import Path

DEFAULT_MAX_AGE_HOURS = 48.0


def cas_root(output_user_root: Path) -> Path:
    """The content-addressable store under a repository cache."""
    return output_user_root / "cache" / "repos" / "v1" / "content_addressable"


@dataclass(frozen=True)
class CasEntry:
    """One cached file in the CAS."""

    path: Path
    atime: float
    size: int
    nlink: int

    @property
    def shared(self) -> bool:
        """Whether this file is hardlinked elsewhere.

        Defensive, not load-bearing: bazel *copies* cache entries into output bases by
        default, and measured here every one of 8964 CAS files had nlink=1. It only
        becomes real under --experimental_repository_cache_hardlinks, where unlinking
        an entry would free nothing until the base holding the other link goes too.
        The check is free (we have already stat'd), so we keep it rather than claim
        space we did not reclaim if that flag ever gets turned on.
        """
        return self.nlink > 1


def iter_entries(root: Path):
    """Yield every file in the CAS."""
    if not root.exists():
        return
    for path in root.rglob("*"):
        try:
            st = path.stat()
        except OSError:
            continue
        if not path.is_file():
            continue
        yield CasEntry(path=path, atime=st.st_atime, size=st.st_size, nlink=st.st_nlink)


def stale(entries, now: float | None = None, max_age_hours: float = DEFAULT_MAX_AGE_HOURS):
    """The entries not read within `max_age_hours`."""
    now = time.time() if now is None else now
    cutoff = now - max_age_hours * 3600.0
    return [e for e in entries if e.atime < cutoff]
