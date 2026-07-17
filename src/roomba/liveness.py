"""Deciding whether an output base is in use right now.

Deleting an output base out from under a running bazel server corrupts the build it
is in the middle of, so liveness is a hard veto in `policy`, never a heuristic to be
weighed. Two independent signals, either of which vetoes:

  * `<base>/lock` -- the bazel server holds an exclusive flock on this for its whole
    life. Probing it is the same mechanism bazel itself uses, so it stays correct
    even if a server dies without cleaning up its pid file.
  * `<base>/server/server.pid.txt` -- covers servers whose lock we can't open (e.g.
    EPERM), and gives us a pid to report.
"""

from __future__ import annotations

import errno
import fcntl
import os
from pathlib import Path


def server_pid(base: Path) -> int | None:
    """The pid recorded by the server owning `base`, if it is still running."""
    try:
        pid = int((base / "server" / "server.pid.txt").read_text().strip())
    except (OSError, ValueError):
        return None
    return pid if pid_alive(pid) else None


def pid_alive(pid: int) -> bool:
    """Whether `pid` names a live process."""
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # Someone else's process: alive, just not ours to signal.
        return True
    return True


def lock_held(base: Path) -> bool:
    """Whether a bazel server holds `<base>/lock`.

    We take the lock only to see whether we can, and drop it immediately. A missing
    lock file means no server ever started here. Anything we can't interpret is
    reported as held: an ambiguous answer must not authorize a delete.
    """
    lock = base / "lock"
    try:
        fd = os.open(lock, os.O_RDWR)
    except FileNotFoundError:
        return False
    except OSError:
        return True
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        return e.errno in (errno.EACCES, errno.EAGAIN)
    else:
        fcntl.flock(fd, fcntl.LOCK_UN)
        return False
    finally:
        os.close(fd)


def in_use(base: Path) -> bool:
    """Whether anything is actively using `base`."""
    return lock_held(base) or server_pid(base) is not None
