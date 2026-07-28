//! roomba -- sweep up after Bazel.
//!
//! Dry-run is the default for every sweep; nothing is deleted without --apply.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::cas;
use crate::layout::{default_output_user_root, iter_output_bases};
use crate::policy::{self, Params};
use crate::reaper::{human, sweep_file, sweep_tree};

#[derive(Parser)]
#[command(name = "roomba", about = "roomba -- sweep up after Bazel.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// report what a sweep would consider, and delete nothing
    Scan(ScanArgs),
    /// remove stale output bases and CAS entries
    Sweep(SweepArgs),
}

#[derive(Args)]
struct Common {
    /// bazel output_user_root to sweep (default: ~/.cache/bazel/_bazel_$USER)
    #[arg(long = "output-user-root")]
    output_user_root: Option<PathBuf>,
    /// retain anything used within this many hours
    #[arg(long = "max-age-hours", default_value_t = policy::DEFAULT_MAX_AGE_HOURS)]
    max_age_hours: f64,
}

impl Common {
    fn root(&self) -> PathBuf {
        self.output_user_root
            .clone()
            .unwrap_or_else(default_output_user_root)
    }
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    common: Common,
    /// list retained bases too, not just sweepable ones
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct SweepArgs {
    #[command(flatten)]
    common: Common,
    /// actually delete; otherwise dry-run
    #[arg(long)]
    apply: bool,
    /// ignore orphans touched this recently, to avoid racing worktree setup
    #[arg(long = "orphan-grace-hours", default_value_t = policy::DEFAULT_ORPHAN_GRACE_HOURS)]
    orphan_grace_hours: f64,
    /// sweep on age alone; ignore whether the workspace exists
    #[arg(long = "no-orphans")]
    no_orphans: bool,
    /// skip the repository cache CAS
    #[arg(long = "bases-only")]
    bases_only: bool,
    /// skip output bases
    #[arg(long = "cas-only")]
    cas_only: bool,
}

fn cmd_scan(args: &ScanArgs) -> i32 {
    let root = args.common.root();
    let judgements = policy::judge_all(
        iter_output_bases(&root),
        Params {
            max_age_hours: args.common.max_age_hours,
            ..Params::default()
        },
    );
    if judgements.is_empty() {
        println!("no output bases under {}", root.display());
        return 0;
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for j in &judgements {
        *counts.entry(j.verdict.value()).or_insert(0) += 1;
    }

    let mut sorted: Vec<&_> = judgements.iter().collect();
    sorted.sort_by(|a, b| {
        a.base
            .last_used
            .partial_cmp(&b.base.last_used)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for j in sorted {
        if args.all || j.sweep() {
            println!("{:>8}  {}  {}", j.verdict.value(), j.base.name(), j.reason);
        }
    }

    println!();
    println!("{} output bases under {}", judgements.len(), root.display());
    for (verdict, n) in &counts {
        println!("  {verdict:>8}  {n}");
    }
    let sweepable = judgements.iter().filter(|j| j.sweep()).count();
    println!("\n{sweepable} would be swept; run `roomba sweep --apply` to do it.");
    0
}

fn cmd_sweep(args: &SweepArgs) -> i32 {
    let root = args.common.root();
    let dry = !args.apply;
    let mut freed = 0u64;
    let mut failures = 0u64;

    if !args.cas_only {
        let judgements = policy::judge_all(
            iter_output_bases(&root),
            Params {
                max_age_hours: args.common.max_age_hours,
                orphan_grace_hours: args.orphan_grace_hours,
                sweep_orphans: !args.no_orphans,
            },
        );
        let doomed: Vec<&_> = judgements.iter().filter(|j| j.sweep()).collect();
        let kept = judgements.len() - doomed.len();
        println!(
            "output bases: {} total, {} retained, {} to sweep",
            judgements.len(),
            kept,
            doomed.len()
        );
        for j in doomed {
            let r = sweep_tree(&j.base.path, dry);
            freed += r.freed;
            if r.ok() {
                let size = if dry {
                    String::new()
                } else {
                    format!("  {}", human(r.freed as f64))
                };
                println!(
                    "  {} {}{}  ({}: {})",
                    if dry { "would sweep" } else { "swept" },
                    j.base.name(),
                    size,
                    j.verdict.value(),
                    j.reason
                );
            } else {
                failures += 1;
                eprintln!(
                    "  FAILED {}: {}",
                    j.base.name(),
                    r.error.unwrap_or_default()
                );
            }
        }
    }

    if !args.bases_only {
        let cas_root = cas::cas_root(&root);
        let doomed_cas = cas::stale(cas::iter_entries(&cas_root), args.common.max_age_hours);
        let mut cas_freed = 0u64;
        for e in &doomed_cas {
            let r = sweep_file(&e.path, dry);
            cas_freed += r.freed;
            if !r.ok() {
                failures += 1;
            }
        }
        freed += cas_freed;
        println!(
            "\nrepository cache: {} entries unread in {}h",
            doomed_cas.len(),
            crate::util::fmt_g(args.common.max_age_hours)
        );
        // Entries bazel still has hardlinked into an output base free nothing until
        // that base goes too, so say so rather than let the bytes look mysteriously
        // short of the entry count.
        let shared = doomed_cas.iter().filter(|e| e.shared()).count();
        if shared > 0 {
            println!(
                "  {shared} still hardlinked into an output base; those free nothing yet -- sweep bases first"
            );
        }
        if !dry {
            println!("  freed {}", human(cas_freed as f64));
        }
    }

    if dry {
        println!(
            "\ndry run -- nothing was deleted, and nothing was measured. re-run with --apply."
        );
    } else {
        println!("\nfreed {} total", human(freed as f64));
    }
    if failures > 0 {
        1
    } else {
        0
    }
}

/// Parse arguments and dispatch, returning the process exit code.
pub fn main() -> i32 {
    let cli = Cli::parse();
    match &cli.command {
        Command::Scan(args) => cmd_scan(args),
        Command::Sweep(args) => cmd_sweep(args),
    }
}
