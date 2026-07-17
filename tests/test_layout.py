"""The census: what counts as an output base, and who does it belong to."""

from roomba.layout import iter_output_bases, last_used, read_workspace

MD5_A = "0" * 32
MD5_B = "1" * 32


def test_reads_workspace_from_do_not_build_here(tmp_path, make_base):
    ws = tmp_path / "workspace"
    ws.mkdir()
    base = make_base(MD5_A, ws)
    assert read_workspace(base) == ws


def test_workspace_is_none_when_marker_missing(output_user_root):
    base = output_user_root / MD5_A
    base.mkdir()
    assert read_workspace(base) is None


def test_iter_skips_cache_and_install_siblings(output_user_root, tmp_path, make_base):
    """The repository cache and install base live beside the output bases.

    Sweeping either would be a disaster, so they must never be enumerated.
    """
    ws = tmp_path / "workspace"
    ws.mkdir()
    make_base(MD5_A, ws)
    (output_user_root / "cache").mkdir()
    (output_user_root / "install").mkdir()
    (output_user_root / "not-an-md5").mkdir()
    (output_user_root / ("f" * 31)).mkdir()  # too short to be an md5

    assert [b.name for b in iter_output_bases(output_user_root)] == [MD5_A]


def test_orphaned_tracks_workspace_existence(output_user_root, tmp_path, make_base):
    ws = tmp_path / "workspace"
    ws.mkdir()
    make_base(MD5_A, ws)
    make_base(MD5_B, tmp_path / "vanished")

    found = {b.name: b for b in iter_output_bases(output_user_root)}
    assert found[MD5_A].orphaned is False
    assert found[MD5_B].orphaned is True


def test_unreadable_workspace_is_not_an_orphan(output_user_root):
    """A base too broken to name its workspace is left to the age rule, not swept."""
    base = output_user_root / MD5_A
    base.mkdir()
    found = next(iter_output_bases(output_user_root))
    assert found.workspace is None
    assert found.orphaned is False


def test_last_used_prefers_newest_marker(tmp_path, make_base, age):
    """A cold base with one recently-touched marker counts as recently used."""
    ws = tmp_path / "workspace"
    ws.mkdir()
    base = make_base(MD5_A, ws, age_hours=100)
    server = base / "server"
    server.mkdir()
    (server / "server.starttime").write_text("x")
    age(server, 100)
    starttime = age(server / "server.starttime", 1)

    assert last_used(base) == starttime.stat().st_mtime
