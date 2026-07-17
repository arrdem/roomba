# roomba

Sweeps up after Bazel. Nothing in here is precious; it all rebuilds.

Every worktree an agent opens gets its own Bazel server and its own output base, and
when the worktree is torn down the output base stays behind — nothing in Bazel ever
collects it. On this workstation that reached 525 output bases against 298 live
worktrees and a disk at 100%. roomba finds the ones nobody can possibly want and
removes them.

## Usage

```sh
bazel run //projects/roomba -- scan            # census; deletes nothing
bazel run //projects/roomba -- sweep           # dry run: what would go, and how much
bazel run //projects/roomba -- sweep --apply   # actually do it
```

Dry-run is the default. `--apply` is the only thing that deletes.

## What it sweeps, and why that's safe

Bazel names each output base `md5(workspace_path)` under `~/.cache/bazel/_bazel_$USER`.
The md5 is one-way, but it never needs inverting: Bazel writes the workspace path
verbatim into `<output_base>/DO_NOT_BUILD_HERE`. That file is an exact reverse
mapping, so "is this base's workspace gone?" is a lookup, not a guess.

Rules, first match wins:

| verdict | meaning | swept |
| --- | --- | --- |
| `in-use` | a Bazel server holds the base | never, at any age |
| `orphaned` | the workspace is gone; nothing can ever use this base again | yes, past a 1h grace |
| `idle` | untouched past the age cutoff (default 48h) | yes |
| `fresh` | recently used, workspace still there | no |

Orphan sweeping is what actually reclaims the disk — agent worktrees die constantly
and their bases are dead the moment they do, regardless of how recently they were
touched. The 1h grace exists so we don't race a worktree that's mid-creation.

Liveness is a hard veto, checked two independent ways: the exclusive flock Bazel
holds on `<base>/lock` for the life of a server, and a live pid in
`<base>/server/server.pid.txt`. Deleting a base under a running server corrupts that
build, so anything ambiguous — an unreadable lock, an unparseable marker — reads as
in-use. A base too broken to name its own workspace is *not* treated as an orphan;
it's left for the age rule, rather than deleted on the strength of a missing file.

Only md5-shaped directories are ever considered, which is what structurally excludes
the `cache/` (repository cache) and `install/` (unpacked Bazel) siblings.

## The repository cache

`sweep` also evicts repository-cache entries (`cache/repos/v1/content_addressable/`)
not *read* in 48h. Bazel never evicts this itself, and everything in it is by
definition re-fetchable — a wrong eviction costs one download over the lab link, not
a rebuild.

One thing worth knowing: these filesystems mount `relatime`, which only refreshes
atime once it's over 24h stale. So an atime can lag real access by up to 24h, and the
48h cutoff can evict something touched ~24h ago. That's a re-download, not a
correctness problem — but it's why the cutoff should stay comfortably above 24h.
Below that, `relatime` makes it meaningless.

## Space accounting

Space is counted *during* the delete, never predicted before it. Measuring a tree up
front means walking every inode to produce a number the delete hands us for free —
over ~500 output bases that's a full `du` of the cache, tens of minutes, for an
answer we'd learn a moment later anyway. So a dry run reports *what* would go and
says nothing about bytes; only `--apply` reports space, which it knows exactly.

The accounting leans on a property of `unlink`: removing one link to a multiply-linked
inode frees nothing. Counting `st_blocks` only when `st_nlink == 1` is therefore
exactly right and needs no dedupe bookkeeping.

That guard is defensive, not load-bearing. Bazel *copies* repository-cache entries
into output bases rather than linking them — measured here, all 8964 CAS files and
every file in a sampled base's `external/` had `nlink=1`. It only becomes real under
`--experimental_repository_cache_hardlinks`. The check is free (the stat is needed to
unlink anyway), so it stays.
