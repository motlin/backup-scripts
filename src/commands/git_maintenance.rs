use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};

use crate::config::{GitMaintenanceConfig, resolve_roots};
use crate::ui::{self, CommandBar, TreeItem, format_duration};
use crate::walk::{dir_size, find_dirs_with_marker};
use humansize::{BINARY, format_size};

pub const DEFAULT_DEPTH: usize = 3;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_SUBMODULES: bool = true;

/// Maintenance steps run in order per repo. Equivalent to `git gc --aggressive` plus
/// reflog/rerere/worktree/commit-graph housekeeping, but without the `-l` flag so that
/// objects from alternates are materialized into the local pack.
///
/// 1. `pack-refs`           pack loose refs into `packed-refs`
/// 2. `reflog expire`       drop reflog entries for objects unreachable for >1 week
/// 3. `rerere gc`           prune resolved-merge cache
/// 4. `worktree prune`      drop stale worktree metadata
/// 5. `repack -Adf`         single-pack, recompute deltas, exile unreachables to loose
/// 6. `prune`               drop loose unreachables older than 1 week
/// 7. `commit-graph write`  rebuild commit-graph + changed-path Bloom filters
///
/// When `--fsck` is passed, `git fsck` runs after the 7 steps to validate repository
/// integrity. Findings (dangling/unreachable/corrupt objects) are reported via the
/// progress tree but do not mark the repo as failed.
const MAINTENANCE_STEPS: &[(&str, &[&str])] = &[
    ("pack-refs", &["pack-refs", "--all", "--prune"]),
    (
        "reflog-expire",
        &[
            "reflog",
            "expire",
            "--all",
            "--expire-unreachable=1.week.ago",
        ],
    ),
    ("rerere-gc", &["rerere", "gc"]),
    ("worktree-prune", &["worktree", "prune"]),
    ("repack", &["repack", "-Adf", "--depth=100", "--window=250"]),
    ("prune", &["prune", "--expire=1.week.ago"]),
    (
        "commit-graph",
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    ),
];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to search for git repositories. [config: git_maintenance.roots or roots; no default — required]
    #[arg(long = "root")]
    roots: Vec<PathBuf>,

    /// Maximum directory depth when searching for repos. [config: git_maintenance.depth, default: 3]
    #[arg(long)]
    depth: Option<usize>,

    /// Maximum number of maintenance sequences to run in parallel. [config: git_maintenance.concurrency, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,

    /// Skip submodules (those whose `.git` is a file pointing to the superproject).
    /// Submodules are included by default. [config: git_maintenance.submodules, default: true]
    #[arg(long)]
    no_submodules: bool,

    /// Run `git fsck` after maintenance to validate repository integrity. Findings are
    /// reported but do not fail the repo. [CLI only; no config field]
    #[arg(long)]
    fsck: bool,
}

pub async fn run(
    args: Args,
    cfg: &GitMaintenanceConfig,
    global_roots: &Option<Vec<PathBuf>>,
    dry_run: bool,
) -> Result<()> {
    let depth = args.depth.or(cfg.depth).unwrap_or(DEFAULT_DEPTH);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let submodules = if args.no_submodules {
        false
    } else {
        cfg.submodules.unwrap_or(DEFAULT_SUBMODULES)
    };
    let fsck = args.fsck;

    let roots = resolve_roots(args.roots, &cfg.roots, global_roots, "git_maintenance")?;

    async move {
        let mut repos: Vec<PathBuf> = Vec::new();
        for root in &roots {
            if !root.exists() {
                warn!("root does not exist: {}", root.display());
                continue;
            }
            repos.extend(find_dirs_with_marker(root, ".git", depth));
        }
        if !submodules {
            repos.retain(|repo| repo.join(".git").is_dir());
        }
        repos.sort();
        repos.dedup();

        info!(repos = repos.len(), submodules, "discovered git repos");

        if dry_run {
            for repo in &repos {
                info!("would maintain {}", repo.display());
            }
            return Ok(());
        }

        if repos.is_empty() {
            return Ok(());
        }

        let bar = Arc::new(CommandBar::new("git-maintenance", repos.len() as u64));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let total_freed = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for repo in repos {
            let sem = Arc::clone(&sem);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let total_freed = Arc::clone(&total_freed);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    maintain_one(repo, &bar, &items, &total_freed, fsck).await;
                }
                .in_current_span(),
            );
        }
        while set.join_next().await.is_some() {}

        let items = Arc::try_unwrap(items)
            .unwrap_or_else(|_| panic!("items arc leaked"))
            .into_inner()
            .unwrap_or_default();
        let ok_count = items.iter().filter(|i| i.ok).count();
        let fail_count = items.len() - ok_count;
        let freed = total_freed.load(std::sync::atomic::Ordering::Relaxed);
        let summary = format!(
            "{ok_count} ok, {fail_count} failed, {} freed",
            format_size(freed, BINARY)
        );

        let bar = Arc::try_unwrap(bar).unwrap_or_else(|_| panic!("bar arc leaked"));
        if fail_count == 0 {
            bar.finish_ok(summary.clone());
        } else {
            bar.finish_err(summary.clone());
        }
        ui::print_tree(&format!("git-maintenance: {summary}"), &items);

        Ok(())
    }
    .instrument(info_span!("git-maintenance"))
    .await
}

async fn maintain_one(
    repo: PathBuf,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    total_freed: &std::sync::atomic::AtomicU64,
    fsck: bool,
) {
    let label = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());

    let gitdir = resolve_gitdir(&repo);
    let size_before = dir_size(&gitdir).await.unwrap_or(0);
    let started = Instant::now();

    let mut failure: Option<(&str, String)> = None;
    for (step_name, step_args) in MAINTENANCE_STEPS {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&repo);
        cmd.args(*step_args);
        match cmd.output().await {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let compact = stderr
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                failure = Some((step_name, compact));
                break;
            }
            Err(e) => {
                failure = Some((step_name, format!("failed to invoke git: {e}")));
                break;
            }
        }
    }

    let fsck_summary = if fsck && failure.is_none() {
        Some(run_fsck(&repo).await)
    } else {
        None
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let elapsed = format_duration(elapsed_ms);
    let size_after = dir_size(&gitdir).await.unwrap_or(size_before);

    let (ok, detail) = if let Some((step, err)) = failure {
        warn!("✗ {label}  {step}: {err} ({elapsed})");
        (false, format!("{step}: {err} ({elapsed})"))
    } else {
        if size_after < size_before {
            total_freed.fetch_add(
                size_before - size_after,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let delta_str = size_delta_str(size_before, size_after);
        let bar_freed = total_freed.load(std::sync::atomic::Ordering::Relaxed);
        bar.set_message(format!("{} freed", format_size(bar_freed, BINARY)));
        let fsck_suffix = match &fsck_summary {
            Some(s) if !s.is_empty() => format!("; fsck: {s}"),
            _ => String::new(),
        };
        info!("✓ {label}  {delta_str} in {elapsed}{fsck_suffix}");
        (true, format!("{delta_str} in {elapsed}{fsck_suffix}"))
    };

    bar.inc(1);
    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

/// Run `git fsck` in `repo` and return a short summary of findings.
///
/// `git fsck` writes findings (dangling/unreachable/missing/corrupt objects) to stderr,
/// one per line like `dangling tree <hash>` or `missing blob <hash>`. We count
/// occurrences by category (first whitespace-separated word) and return a compact
/// summary like `3 dangling, 1 missing`. Returns an empty string when the repo is clean.
async fn run_fsck(repo: &std::path::Path) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd.args(["fsck", "--full", "--unreachable", "--dangling"]);
    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return format!("failed to invoke git fsck: {e}"),
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for line in stderr.lines() {
        if let Some(category) = line.split_whitespace().next() {
            *counts.entry(category).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    counts
        .into_iter()
        .map(|(k, v)| format!("{v} {k}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn size_delta_str(before: u64, after: u64) -> String {
    if after < before {
        format!("freed {}", format_size(before - after, BINARY))
    } else if after > before {
        format!("+{}", format_size(after - before, BINARY))
    } else {
        format!("no change ({})", format_size(before, BINARY))
    }
}

/// Resolve the actual `.git` directory, following the `gitdir: <path>` pointer for submodules.
fn resolve_gitdir(repo: &std::path::Path) -> PathBuf {
    let dotgit = repo.join(".git");
    if dotgit.is_dir() {
        return dotgit;
    }
    if let Ok(content) = std::fs::read_to_string(&dotgit)
        && let Some(path) = content
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("gitdir: "))
    {
        let p = std::path::Path::new(path);
        return if p.is_absolute() {
            PathBuf::from(p)
        } else {
            dotgit.parent().unwrap_or(std::path::Path::new(".")).join(p)
        };
    }
    dotgit
}
