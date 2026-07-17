"""The rules, and the safety properties that must hold regardless of the rules."""

import fcntl
import os

from roomba.layout import iter_output_bases
from roomba.policy import Verdict, judge_all

MD5_A = "0" * 32
MD5_B = "1" * 32


def _judge(root, **kwargs):
    return {j.base.name: j for j in judge_all(iter_output_bases(root), **kwargs)}


def test_fresh_base_is_kept(output_user_root, tmp_path, make_base):
    ws = tmp_path / "workspace"
    ws.mkdir()
    make_base(MD5_A, ws, age_hours=1)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.FRESH


def test_idle_base_past_cutoff_is_swept(output_user_root, tmp_path, make_base):
    ws = tmp_path / "workspace"
    ws.mkdir()
    make_base(MD5_A, ws, age_hours=72)
    j = _judge(output_user_root)[MD5_A]
    assert j.verdict is Verdict.IDLE
    assert j.sweep


def test_cutoff_boundary_is_inclusive(output_user_root, tmp_path, make_base):
    ws = tmp_path / "workspace"
    ws.mkdir()
    make_base(MD5_A, ws, age_hours=47.9)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.FRESH
    assert _judge(output_user_root, max_age_hours=24)[MD5_A].verdict is Verdict.IDLE


def test_orphan_is_swept_even_when_recently_used(output_user_root, tmp_path, make_base):
    """The whole point: an agent worktree torn down an hour ago is dead weight."""
    make_base(MD5_A, tmp_path / "vanished", age_hours=2)
    j = _judge(output_user_root)[MD5_A]
    assert j.verdict is Verdict.ORPHANED
    assert j.sweep


def test_orphan_inside_grace_period_is_kept(output_user_root, tmp_path, make_base):
    """Don't race a worktree that is mid-creation."""
    make_base(MD5_A, tmp_path / "not-yet", age_hours=0.1)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.FRESH


def test_no_orphans_falls_back_to_pure_age(output_user_root, tmp_path, make_base):
    make_base(MD5_A, tmp_path / "vanished", age_hours=2)
    j = _judge(output_user_root, sweep_orphans=False)[MD5_A]
    assert j.verdict is Verdict.FRESH


def test_live_server_is_never_swept_even_when_orphaned_and_ancient(output_user_root, tmp_path, make_base):
    """Liveness is a hard veto -- deleting a base under a running server corrupts it."""
    base = make_base(MD5_A, tmp_path / "vanished", age_hours=1000)
    fd = os.open(base / "lock", os.O_RDWR)
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    try:
        j = _judge(output_user_root)[MD5_A]
        assert j.verdict is Verdict.IN_USE
        assert not j.sweep
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def test_unlocked_base_is_not_in_use(output_user_root, tmp_path, make_base):
    """An abandoned lock file must not pin a base forever."""
    make_base(MD5_A, tmp_path / "vanished", age_hours=72)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.ORPHANED


def _with_pid(base, pid, age):
    server = base / "server"
    server.mkdir()
    (server / "server.pid.txt").write_text(str(pid))
    age(server, 72)
    return base


def test_live_pid_vetoes_sweep(output_user_root, tmp_path, make_base, age):
    base = make_base(MD5_A, tmp_path / "vanished", age_hours=72, lock=False)
    _with_pid(base, os.getpid(), age)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.IN_USE


def test_stale_pid_does_not_veto_sweep(output_user_root, tmp_path, make_base, age):
    """A server that died without cleaning up must not pin its base forever."""
    base = make_base(MD5_A, tmp_path / "vanished", age_hours=72, lock=False)
    _with_pid(base, 999999999, age)
    assert _judge(output_user_root)[MD5_A].verdict is Verdict.ORPHANED
