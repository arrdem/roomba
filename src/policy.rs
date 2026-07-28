//! What to keep, what to sweep, and why.
//!
//! Policy is pure: it reads facts and returns a verdict per output base, so the rules
//! can be tested without a filesystem full of bazel servers and the CLI can print the
//! same reasons in dry-run that it acts on in --apply.
//!
//! Rule order matters -- the first match wins:
//!
//! 1. IN_USE -- a server holds the base. Hard veto; never swept, at any age.
//! 2. ORPHANED -- the workspace is gone, so nothing can ever use this base again.
//!    Swept once past a short grace period, regardless of age. This is the rule that
//!    actually reclaims the disk: agent worktrees are torn down constantly and leave
//!    their bases behind.
//! 3. IDLE -- untouched past the age cutoff. The plain LRU sweep.
//! 4. FRESH -- recently used and still attached to a live workspace. Kept.
//!
//! The grace period exists for a narrow race: a worktree that is being created, whose
//! bazel server has started but whose directory is momentarily absent, would otherwise
//! read as an orphan and be swept mid-build.

use crate::layout::OutputBase;
use crate::liveness::in_use;
use crate::util::{fmt_g, now};

pub const DEFAULT_MAX_AGE_HOURS: f64 = 48.0;
pub const DEFAULT_ORPHAN_GRACE_HOURS: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    InUse,
    Orphaned,
    Idle,
    Fresh,
}

impl Verdict {
    /// The stable string label, matching the Python enum values.
    pub fn value(self) -> &'static str {
        match self {
            Verdict::InUse => "in-use",
            Verdict::Orphaned => "orphaned",
            Verdict::Idle => "idle",
            Verdict::Fresh => "fresh",
        }
    }

    pub fn sweep(self) -> bool {
        matches!(self, Verdict::Orphaned | Verdict::Idle)
    }
}

#[derive(Debug, Clone)]
pub struct Judgement {
    pub base: OutputBase,
    pub verdict: Verdict,
    pub reason: String,
}

impl Judgement {
    pub fn sweep(&self) -> bool {
        self.verdict.sweep()
    }
}

/// Tunables for [`judge`], mirroring the Python keyword arguments and their defaults.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub max_age_hours: f64,
    pub orphan_grace_hours: f64,
    pub sweep_orphans: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            max_age_hours: DEFAULT_MAX_AGE_HOURS,
            orphan_grace_hours: DEFAULT_ORPHAN_GRACE_HOURS,
            sweep_orphans: true,
        }
    }
}

/// Decide the fate of a single output base against a fixed `now`.
pub fn judge(base: OutputBase, now: f64, p: Params) -> Judgement {
    let age_hours = (now - base.last_used) / 3600.0;

    if in_use(&base.path) {
        return Judgement {
            base,
            verdict: Verdict::InUse,
            reason: "bazel server is running here".to_string(),
        };
    }

    if base.orphaned() && p.sweep_orphans {
        // orphaned() is only true when workspace is Some, so this is safe to render.
        let ws = base
            .workspace
            .as_ref()
            .map(|w| w.display().to_string())
            .unwrap_or_default();
        if age_hours < p.orphan_grace_hours {
            let reason = format!(
                "workspace {ws} is missing but the base was touched {age_hours:.1}h ago, \
                 inside the {}h grace period",
                fmt_g(p.orphan_grace_hours),
            );
            return Judgement {
                base,
                verdict: Verdict::Fresh,
                reason,
            };
        }
        return Judgement {
            base,
            verdict: Verdict::Orphaned,
            reason: format!("workspace {ws} no longer exists"),
        };
    }

    if age_hours >= p.max_age_hours {
        let reason = format!(
            "unused for {age_hours:.1}h (cutoff {}h)",
            fmt_g(p.max_age_hours)
        );
        return Judgement {
            base,
            verdict: Verdict::Idle,
            reason,
        };
    }

    Judgement {
        base,
        verdict: Verdict::Fresh,
        reason: format!("used {age_hours:.1}h ago"),
    }
}

/// Judge every base against a single consistent `now`.
pub fn judge_all(bases: Vec<OutputBase>, p: Params) -> Vec<Judgement> {
    let now = now();
    bases.into_iter().map(|b| judge(b, now, p)).collect()
}

#[cfg(test)]
mod tests {
    //! The rules, and the safety properties that must hold regardless of the rules.
    use super::*;
    use crate::layout::iter_output_bases;
    use crate::testutil::{age, make_base, output_user_root, TmpDir};
    use std::collections::HashMap;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const MD5_A: &str = "00000000000000000000000000000000";

    fn judged(root: &Path, p: Params) -> HashMap<String, Judgement> {
        judge_all(iter_output_bases(root), p)
            .into_iter()
            .map(|j| (j.base.name(), j))
            .collect()
    }

    #[test]
    fn fresh_base_is_kept() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        make_base(&root, MD5_A, Some(&ws), 1.0, true);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::Fresh
        );
    }

    #[test]
    fn idle_base_past_cutoff_is_swept() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        make_base(&root, MD5_A, Some(&ws), 72.0, true);
        let j = &judged(&root, Params::default())[MD5_A];
        assert_eq!(j.verdict, Verdict::Idle);
        assert!(j.sweep());
    }

    #[test]
    fn cutoff_boundary_is_inclusive() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let ws = tmp.join("workspace");
        std::fs::create_dir(&ws).unwrap();
        make_base(&root, MD5_A, Some(&ws), 47.9, true);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::Fresh
        );
        let p = Params {
            max_age_hours: 24.0,
            ..Params::default()
        };
        assert_eq!(judged(&root, p)[MD5_A].verdict, Verdict::Idle);
    }

    #[test]
    fn orphan_is_swept_even_when_recently_used() {
        // The whole point: an agent worktree torn down an hour ago is dead weight.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        make_base(&root, MD5_A, Some(&tmp.join("vanished")), 2.0, true);
        let j = &judged(&root, Params::default())[MD5_A];
        assert_eq!(j.verdict, Verdict::Orphaned);
        assert!(j.sweep());
    }

    #[test]
    fn orphan_inside_grace_period_is_kept() {
        // Don't race a worktree that is mid-creation.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        make_base(&root, MD5_A, Some(&tmp.join("not-yet")), 0.1, true);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::Fresh
        );
    }

    #[test]
    fn no_orphans_falls_back_to_pure_age() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        make_base(&root, MD5_A, Some(&tmp.join("vanished")), 2.0, true);
        let p = Params {
            sweep_orphans: false,
            ..Params::default()
        };
        assert_eq!(judged(&root, p)[MD5_A].verdict, Verdict::Fresh);
    }

    #[test]
    fn live_server_is_never_swept_even_when_orphaned_and_ancient() {
        // Liveness is a hard veto -- deleting a base under a running server corrupts it.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let base = make_base(&root, MD5_A, Some(&tmp.join("vanished")), 1000.0, true);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(base.join("lock"))
            .unwrap();
        let fd = f.as_raw_fd();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let j = &judged(&root, Params::default())[MD5_A];
        assert_eq!(j.verdict, Verdict::InUse);
        assert!(!j.sweep());
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
    }

    #[test]
    fn unlocked_base_is_not_in_use() {
        // An abandoned lock file must not pin a base forever.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        make_base(&root, MD5_A, Some(&tmp.join("vanished")), 72.0, true);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::Orphaned
        );
    }

    fn with_pid(base: &Path, pid: i32) {
        let server = base.join("server");
        std::fs::create_dir(&server).unwrap();
        std::fs::write(server.join("server.pid.txt"), pid.to_string()).unwrap();
        age(&server, 72.0);
    }

    #[test]
    fn live_pid_vetoes_sweep() {
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let base = make_base(&root, MD5_A, Some(&tmp.join("vanished")), 72.0, false);
        with_pid(&base, std::process::id() as i32);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::InUse
        );
    }

    #[test]
    fn stale_pid_does_not_veto_sweep() {
        // A server that died without cleaning up must not pin its base forever.
        let tmp = TmpDir::new();
        let root = output_user_root(&tmp);
        let base = make_base(&root, MD5_A, Some(&tmp.join("vanished")), 72.0, false);
        with_pid(&base, 999999999);
        assert_eq!(
            judged(&root, Params::default())[MD5_A].verdict,
            Verdict::Orphaned
        );
    }
}
