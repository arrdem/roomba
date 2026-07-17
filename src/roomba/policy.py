"""What to keep, what to sweep, and why.

Policy is pure: it reads facts and returns a verdict per output base, so the rules
can be tested without a filesystem full of bazel servers and the CLI can print the
same reasons in dry-run that it acts on in --apply.

Rule order matters -- the first match wins:

  1. IN_USE      a server holds the base. Hard veto; never swept, at any age.
  2. ORPHANED    the workspace is gone, so nothing can ever use this base again.
                 Swept once past a short grace period, regardless of age. This is
                 the rule that actually reclaims the disk: agent worktrees are torn
                 down constantly and leave their bases behind.
  3. IDLE        untouched past the age cutoff. The plain LRU sweep.
  4. FRESH       recently used and still attached to a live workspace. Kept.

The grace period exists for a narrow race: a worktree that is being created, whose
bazel server has started but whose directory is momentarily absent, would otherwise
read as an orphan and be swept mid-build.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from enum import Enum

from roomba.layout import OutputBase
from roomba.liveness import in_use

DEFAULT_MAX_AGE_HOURS = 48.0
DEFAULT_ORPHAN_GRACE_HOURS = 1.0


class Verdict(Enum):
    IN_USE = "in-use"
    ORPHANED = "orphaned"
    IDLE = "idle"
    FRESH = "fresh"

    @property
    def sweep(self) -> bool:
        return self in (Verdict.ORPHANED, Verdict.IDLE)


@dataclass(frozen=True)
class Judgement:
    base: OutputBase
    verdict: Verdict
    reason: str

    @property
    def sweep(self) -> bool:
        return self.verdict.sweep


def judge(
    base: OutputBase,
    now: float | None = None,
    max_age_hours: float = DEFAULT_MAX_AGE_HOURS,
    orphan_grace_hours: float = DEFAULT_ORPHAN_GRACE_HOURS,
    sweep_orphans: bool = True,
) -> Judgement:
    """Decide the fate of a single output base."""
    now = time.time() if now is None else now
    age_hours = (now - base.last_used) / 3600.0

    if in_use(base.path):
        return Judgement(base, Verdict.IN_USE, "bazel server is running here")

    if base.orphaned and sweep_orphans:
        if age_hours < orphan_grace_hours:
            return Judgement(
                base,
                Verdict.FRESH,
                f"workspace {base.workspace} is missing but the base was touched "
                f"{age_hours:.1f}h ago, inside the {orphan_grace_hours:g}h grace period",
            )
        return Judgement(base, Verdict.ORPHANED, f"workspace {base.workspace} no longer exists")

    if age_hours >= max_age_hours:
        return Judgement(base, Verdict.IDLE, f"unused for {age_hours:.1f}h (cutoff {max_age_hours:g}h)")

    return Judgement(base, Verdict.FRESH, f"used {age_hours:.1f}h ago")


def judge_all(bases, **kwargs) -> list[Judgement]:
    """Judge every base against a single consistent `now`."""
    now = kwargs.pop("now", None) or time.time()
    return [judge(base, now=now, **kwargs) for base in bases]
