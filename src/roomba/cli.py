"""roomba -- sweep up after Bazel.

Dry-run is the default for every sweep; nothing is deleted without --apply.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from roomba import cas as cas_mod
from roomba import policy
from roomba.layout import default_output_user_root, iter_output_bases
from roomba.reaper import human, sweep_file, sweep_tree


def _common(p: argparse.ArgumentParser) -> None:
    p.add_argument(
        "--output-user-root",
        type=Path,
        default=default_output_user_root(),
        help="bazel output_user_root to sweep (default: %(default)s)",
    )
    p.add_argument(
        "--max-age-hours",
        type=float,
        default=policy.DEFAULT_MAX_AGE_HOURS,
        help="retain anything used within this many hours (default: %(default)s)",
    )


def _parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="roomba", description=__doc__.splitlines()[0])
    subs = p.add_subparsers(dest="command", required=True)

    scan = subs.add_parser("scan", help="report what a sweep would consider, and delete nothing")
    _common(scan)
    scan.add_argument("--all", action="store_true", help="list retained bases too, not just sweepable ones")

    sweep = subs.add_parser("sweep", help="remove stale output bases and CAS entries")
    _common(sweep)
    sweep.add_argument("--apply", action="store_true", help="actually delete; otherwise dry-run")
    sweep.add_argument(
        "--orphan-grace-hours",
        type=float,
        default=policy.DEFAULT_ORPHAN_GRACE_HOURS,
        help="ignore orphans touched this recently, to avoid racing worktree setup (default: %(default)s)",
    )
    sweep.add_argument(
        "--no-orphans", action="store_true", help="sweep on age alone; ignore whether the workspace exists"
    )
    sweep.add_argument("--bases-only", action="store_true", help="skip the repository cache CAS")
    sweep.add_argument("--cas-only", action="store_true", help="skip output bases")
    return p


def cmd_scan(args: argparse.Namespace) -> int:
    judgements = policy.judge_all(
        iter_output_bases(args.output_user_root),
        max_age_hours=args.max_age_hours,
    )
    if not judgements:
        print(f"no output bases under {args.output_user_root}")
        return 0

    counts: dict[str, int] = {}
    for j in judgements:
        counts[j.verdict.value] = counts.get(j.verdict.value, 0) + 1

    for j in sorted(judgements, key=lambda j: j.base.last_used):
        if args.all or j.sweep:
            print(f"{j.verdict.value:>8}  {j.base.name}  {j.reason}")

    print()
    print(f"{len(judgements)} output bases under {args.output_user_root}")
    for verdict, n in sorted(counts.items()):
        print(f"  {verdict:>8}  {n}")
    sweepable = sum(1 for j in judgements if j.sweep)
    print(f"\n{sweepable} would be swept; run `roomba sweep --apply` to do it.")
    return 0


def cmd_sweep(args: argparse.Namespace) -> int:
    dry = not args.apply
    freed = 0
    failures = 0

    if not args.cas_only:
        judgements = policy.judge_all(
            iter_output_bases(args.output_user_root),
            max_age_hours=args.max_age_hours,
            orphan_grace_hours=args.orphan_grace_hours,
            sweep_orphans=not args.no_orphans,
        )
        doomed = [j for j in judgements if j.sweep]
        kept = len(judgements) - len(doomed)
        print(f"output bases: {len(judgements)} total, {kept} retained, {len(doomed)} to sweep")
        for j in doomed:
            r = sweep_tree(j.base.path, dry_run=dry)
            freed += r.freed
            if r.ok:
                size = "" if dry else f"  {human(r.freed)}"
                print(f"  {'would sweep' if dry else 'swept'} {j.base.name}{size}  ({j.verdict.value}: {j.reason})")
            else:
                failures += 1
                print(f"  FAILED {j.base.name}: {r.error}", file=sys.stderr)

    if not args.bases_only:
        root = cas_mod.cas_root(args.output_user_root)
        doomed_cas = cas_mod.stale(cas_mod.iter_entries(root), max_age_hours=args.max_age_hours)
        cas_freed = 0
        for e in doomed_cas:
            r = sweep_file(e.path, dry_run=dry)
            cas_freed += r.freed
            if not r.ok:
                failures += 1
        freed += cas_freed
        print(f"\nrepository cache: {len(doomed_cas)} entries unread in {args.max_age_hours:g}h")
        # Entries bazel still has hardlinked into an output base free nothing until
        # that base goes too, so say so rather than let the bytes look mysteriously
        # short of the entry count.
        shared = sum(1 for e in doomed_cas if e.shared)
        if shared:
            print(f"  {shared} still hardlinked into an output base; those free nothing yet -- sweep bases first")
        if not dry:
            print(f"  freed {human(cas_freed)}")

    if dry:
        print("\ndry run -- nothing was deleted, and nothing was measured. re-run with --apply.")
    else:
        print(f"\nfreed {human(freed)} total")
    return 1 if failures else 0


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.command == "scan":
        return cmd_scan(args)
    return cmd_sweep(args)


if __name__ == "__main__":
    sys.exit(main())
