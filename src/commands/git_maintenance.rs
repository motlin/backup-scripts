use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::{GitMaintenanceConfig, GitMaintenanceOverride, resolve_roots};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
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

/// Reject overrides that name a step which doesn't exist, so a typo (e.g.
/// `gc` instead of `repack`) fails loudly instead of silently doing nothing.
fn validate_overrides(overrides: &[GitMaintenanceOverride]) -> Result<()> {
    let known: HashSet<&str> = MAINTENANCE_STEPS.iter().map(|(name, _)| *name).collect();
    for over in overrides {
        for step in &over.skip_steps {
            if !known.contains(step.as_str()) {
                bail!(
                    "unknown step in git_maintenance.overrides for `{}`: {step} (valid: {})",
                    over.repo,
                    MAINTENANCE_STEPS
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(())
}

/// True when `repo`'s trailing path components equal those of `suffix`. Compared
/// component-wise so `monorepo` does not match a repo named `huge-monorepo`.
fn matches_suffix(repo: &Path, suffix: &str) -> bool {
    let suffix_comps: Vec<_> = Path::new(suffix).components().collect();
    if suffix_comps.is_empty() {
        return false;
    }
    let repo_comps: Vec<_> = repo.components().collect();
    if suffix_comps.len() > repo_comps.len() {
        return false;
    }
    repo_comps[repo_comps.len() - suffix_comps.len()..] == suffix_comps[..]
}

/// Union of all `skip_steps` from overrides whose `repo` suffix matches `repo`.
fn skip_steps_for(repo: &Path, overrides: &[GitMaintenanceOverride]) -> HashSet<String> {
    overrides
        .iter()
        .filter(|over| matches_suffix(repo, &over.repo))
        .flat_map(|over| over.skip_steps.iter().cloned())
        .collect()
}

/// Skipped step names in canonical maintenance order, for stable reporting.
fn ordered_skipped(skip: &HashSet<String>) -> Vec<&'static str> {
    MAINTENANCE_STEPS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| skip.contains(*name))
        .collect()
}

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
) -> Result<CommandSummary> {
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
    let overrides = cfg.overrides.clone().unwrap_or_default();
    validate_overrides(&overrides)?;

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

        // Dedup by git common dir so linked worktrees of the same repo (which share
        // the object database, packed-refs, reflogs, and commit-graph) only get one
        // maintenance pass — running repack/prune on each would race on the shared
        // packfiles and double-count freed bytes. Submodules have distinct common
        // dirs and are preserved.
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut deduped: Vec<PathBuf> = Vec::new();
        for repo in repos {
            match git_common_dir(&repo).await {
                Some(common) => {
                    if seen.insert(common) {
                        deduped.push(repo);
                    }
                }
                None => {
                    warn!("could not resolve git common dir for {}", repo.display());
                    deduped.push(repo);
                }
            }
        }
        let repos = deduped;

        info!(repos = repos.len(), submodules, "discovered git repos");

        if dry_run {
            for repo in &repos {
                let skip = skip_steps_for(repo, &overrides);
                if skip.is_empty() {
                    info!("would maintain {}", repo.display());
                } else {
                    info!(
                        "would maintain {} (skip: {})",
                        repo.display(),
                        ordered_skipped(&skip).join(", ")
                    );
                }
            }
            return Ok(CommandSummary {
                items_ok: repos.len() as u64,
                ..CommandSummary::default()
            });
        }

        if repos.is_empty() {
            return Ok(CommandSummary::default());
        }

        let bar = Arc::new(CommandBar::new("git-maintenance", repos.len() as u64));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let totals = Arc::new(Totals::default());

        let max_label = repos
            .iter()
            .map(|r| repo_label(r).chars().count())
            .max()
            .unwrap_or(0);

        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for repo in repos {
            let skip = skip_steps_for(&repo, &overrides);
            let sem = Arc::clone(&sem);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let totals = Arc::clone(&totals);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    maintain_one(repo, max_label, &bar, &items, &totals, fsck, &skip).await;
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
        let reclaimed = totals.reclaimed.load(std::sync::atomic::Ordering::Relaxed);
        let added = totals.added.load(std::sync::atomic::Ordering::Relaxed);
        let net = reclaimed.saturating_sub(added);
        let summary = format!(
            "{ok_count} ok, {fail_count} failed, {}",
            freed_summary(reclaimed, added)
        );

        let bar = Arc::try_unwrap(bar).unwrap_or_else(|_| panic!("bar arc leaked"));
        if fail_count == 0 {
            bar.finish_ok(summary.clone());
        } else {
            bar.finish_err(summary.clone());
        }
        ui::print_tree(&format!("git-maintenance: {summary}"), &items);

        Ok(CommandSummary {
            bytes_freed: net,
            items_ok: ok_count as u64,
            items_failed: fail_count as u64,
            items_skipped: 0,
        })
    }
    .instrument(info_span!("git-maintenance"))
    .await
}

fn repo_label(repo: &std::path::Path) -> String {
    repo.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string())
}

/// Running byte totals across all successfully-maintained repos: gross bytes
/// reclaimed (repos that shrank) and gross bytes added back (repos that grew).
/// The net freed figure is `reclaimed - added`, saturating at zero.
#[derive(Default)]
struct Totals {
    reclaimed: std::sync::atomic::AtomicU64,
    added: std::sync::atomic::AtomicU64,
}

async fn maintain_one(
    repo: PathBuf,
    max_label: usize,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    totals: &Totals,
    fsck: bool,
    skip: &HashSet<String>,
) {
    let label = repo_label(&repo);
    let padded = pad_right(&label, max_label);

    let gitdir = resolve_gitdir(&repo);
    let size_before = dir_size(&gitdir).await.unwrap_or(0);
    let started = Instant::now();

    let mut failure: Option<(&str, String)> = None;
    for (step_name, step_args) in MAINTENANCE_STEPS {
        if skip.contains(*step_name) {
            continue;
        }
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
        let suffix = format!("{step}: {err} ({elapsed})");
        let detail = ItemDetail::failure(suffix);
        warn!("✗ {padded}  {}", ui::format_detail(&detail));
        (false, detail)
    } else {
        if size_after < size_before {
            totals.reclaimed.fetch_add(
                size_before - size_after,
                std::sync::atomic::Ordering::Relaxed,
            );
        } else if size_after > size_before {
            totals.added.fetch_add(
                size_after - size_before,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let (verb, size_str) = delta_components(size_before, size_after);
        let reclaimed = totals.reclaimed.load(std::sync::atomic::Ordering::Relaxed);
        let added = totals.added.load(std::sync::atomic::Ordering::Relaxed);
        bar.set_message(net_phrase(reclaimed, added));
        let mut notes = String::new();
        let skipped = ordered_skipped(skip);
        if !skipped.is_empty() {
            notes.push_str(&format!("; skipped: {}", skipped.join(", ")));
        }
        if let Some(s) = &fsck_summary
            && !s.is_empty()
        {
            notes.push_str(&format!("; fsck: {s}"));
        }
        let mut detail = ItemDetail::success(verb, size_str, &elapsed);
        if !notes.is_empty() {
            detail = detail.with_suffix(notes);
        }
        info!("✓ {padded}  {}", ui::format_detail(&detail));
        (true, detail)
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

/// Short net phrase for the live bar and the lead of the summary.
fn net_phrase(reclaimed: u64, added: u64) -> String {
    if added > reclaimed {
        format!("grew by {}", format_size(added - reclaimed, BINARY))
    } else {
        format!("{} freed", format_size(reclaimed - added, BINARY))
    }
}

/// Full summary fragment: net phrase plus a reclaimed/added breakdown when
/// any repo grew during maintenance.
fn freed_summary(reclaimed: u64, added: u64) -> String {
    let net = net_phrase(reclaimed, added);
    if added == 0 {
        net
    } else {
        format!(
            "{net} ({} reclaimed, {} added back)",
            format_size(reclaimed, BINARY),
            format_size(added, BINARY),
        )
    }
}

fn delta_components(before: u64, after: u64) -> (&'static str, String) {
    if after < before {
        ("freed", format_size(before - after, BINARY))
    } else if after > before {
        ("+", format_size(after - before, BINARY))
    } else {
        ("no change", format_size(before, BINARY))
    }
}

/// Resolve `repo`'s git common dir (the shared object store) via
/// `git rev-parse --git-common-dir`. Linked worktrees of the same repo all return
/// the same path here even though their per-worktree gitdirs differ; submodules
/// return their own modules dir.
async fn git_common_dir(repo: &std::path::Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(stdout.trim());
    Some(std::fs::canonicalize(&path).unwrap_or(path))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn over(repo: &str, skip: &[&str]) -> GitMaintenanceOverride {
        GitMaintenanceOverride {
            repo: repo.to_string(),
            skip_steps: skip.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn matches_suffix_full_and_partial_components() {
        let repo = Path::new("/Users/me/projects/huge-monorepo");
        assert!(matches_suffix(repo, "huge-monorepo"));
        assert!(matches_suffix(repo, "projects/huge-monorepo"));
        assert!(matches_suffix(repo, "/Users/me/projects/huge-monorepo"));
    }

    #[test]
    fn matches_suffix_rejects_non_component_substring() {
        let repo = Path::new("/Users/me/projects/huge-monorepo");
        assert!(!matches_suffix(repo, "monorepo"));
        assert!(!matches_suffix(repo, "other-repo"));
        assert!(!matches_suffix(repo, ""));
    }

    #[test]
    fn matches_suffix_rejects_suffix_longer_than_repo() {
        assert!(!matches_suffix(Path::new("/a/b"), "x/a/b"));
    }

    #[test]
    fn skip_steps_for_unions_matching_overrides() {
        let repo = Path::new("/Users/me/projects/huge-monorepo");
        let overrides = vec![
            over("projects/huge-monorepo", &["repack"]),
            over("huge-monorepo", &["prune"]),
            over("other", &["commit-graph"]),
        ];
        let skip = skip_steps_for(repo, &overrides);
        assert_eq!(
            skip,
            HashSet::from(["repack".to_string(), "prune".to_string()])
        );
    }

    #[test]
    fn skip_steps_for_no_match_is_empty() {
        let repo = Path::new("/Users/me/projects/small");
        let overrides = vec![over("huge-monorepo", &["repack"])];
        assert!(skip_steps_for(repo, &overrides).is_empty());
    }

    #[test]
    fn validate_overrides_accepts_known_steps() {
        let overrides = vec![over("a", &["repack", "prune", "commit-graph"])];
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn validate_overrides_rejects_unknown_step() {
        let overrides = vec![over("a", &["gc"])];
        let err = validate_overrides(&overrides).unwrap_err().to_string();
        assert!(err.contains("gc"), "error should name the bad step: {err}");
    }

    #[test]
    fn net_phrase_shrink_equal_grow() {
        assert_eq!(net_phrase(1024, 0), "1 KiB freed");
        assert_eq!(net_phrase(1048576, 1048576), "0 B freed");
        assert_eq!(net_phrase(0, 0), "0 B freed");
        assert_eq!(net_phrase(1048576, 3 * 1048576), "grew by 2 MiB");
    }

    #[test]
    fn freed_summary_no_growth_omits_parenthetical() {
        assert_eq!(freed_summary(1024, 0), "1 KiB freed");
        assert_eq!(freed_summary(0, 0), "0 B freed");
    }

    #[test]
    fn freed_summary_net_positive_with_growth() {
        assert_eq!(
            freed_summary(3 * 1048576, 1048576),
            "2 MiB freed (3 MiB reclaimed, 1 MiB added back)"
        );
    }

    #[test]
    fn freed_summary_net_negative_with_growth() {
        assert_eq!(
            freed_summary(1048576, 3 * 1048576),
            "grew by 2 MiB (1 MiB reclaimed, 3 MiB added back)"
        );
    }
}
