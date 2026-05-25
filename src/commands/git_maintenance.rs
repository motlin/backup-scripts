use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, info_span, warn, Instrument};

use crate::config::{resolve_roots, GitMaintenanceConfig};
use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::{dir_size, find_dirs_with_marker};
use humansize::{format_size, BINARY};

pub const DEFAULT_DEPTH: usize = 3;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_SUBMODULES: bool = true;
pub const DEFAULT_TASKS: &[&str] = &[
    "commit-graph",
    "incremental-repack",
    "loose-objects",
    "pack-refs",
];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to search for git repositories. [config: git_maintenance.roots or roots; no default — required]
    #[arg(long = "root")]
    roots: Vec<PathBuf>,

    /// Maximum directory depth when searching for repos. [config: git_maintenance.depth, default: 3]
    #[arg(long)]
    depth: Option<usize>,

    /// Maximum number of `git maintenance` invocations to run in parallel. [config: git_maintenance.concurrency, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,

    /// Tasks to run (repeatable). [config: git_maintenance.tasks, default: commit-graph, incremental-repack, loose-objects, pack-refs]
    #[arg(long = "task")]
    tasks: Vec<String>,

    /// Also run the `prefetch` task. Fetches every remote of every repo — slow and network-bound.
    /// [config: git_maintenance.prefetch, default: false]
    #[arg(long)]
    prefetch: bool,

    /// Skip submodules (those whose `.git` is a file pointing to the superproject).
    /// Submodules are included by default. [config: git_maintenance.submodules, default: true]
    #[arg(long)]
    no_submodules: bool,
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

    let mut tasks: Vec<String> = if !args.tasks.is_empty() {
        args.tasks.clone()
    } else if let Some(t) = cfg.tasks.clone() {
        t
    } else {
        DEFAULT_TASKS.iter().map(|s| s.to_string()).collect()
    };
    let prefetch = args.prefetch || cfg.prefetch.unwrap_or(false);
    if prefetch && !tasks.iter().any(|t| t == "prefetch") {
        tasks.push("prefetch".to_string());
    }
    let submodules = if args.no_submodules {
        false
    } else {
        cfg.submodules.unwrap_or(DEFAULT_SUBMODULES)
    };

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

        info!(
            repos = repos.len(),
            tasks = ?tasks,
            submodules,
            "discovered git repos"
        );

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
        let tasks = Arc::new(tasks);
        let mut set: JoinSet<()> = JoinSet::new();
        for repo in repos {
            let sem = Arc::clone(&sem);
            let tasks = Arc::clone(&tasks);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let total_freed = Arc::clone(&total_freed);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    maintain_one(repo, &tasks, &bar, &items, &total_freed).await;
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
    tasks: &[String],
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    total_freed: &std::sync::atomic::AtomicU64,
) {
    let label = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());

    let pack_dir = resolve_pack_dir(&repo);
    let has_packs = has_pack_files(&pack_dir);
    let effective_tasks: Vec<&String> = tasks
        .iter()
        .filter(|t| t.as_str() != "incremental-repack" || has_packs)
        .collect();

    if effective_tasks.is_empty() {
        info!("• {label}  nothing to do (no preconditions met)");
        bar.inc(1);
        items.lock().unwrap().push(TreeItem {
            label,
            detail: "nothing to do".into(),
            ok: true,
        });
        return;
    }

    // Measure size delta of the .git directory (or resolved gitdir for submodules).
    let gitdir = resolve_gitdir(&repo);
    let size_before = dir_size(&gitdir).await.unwrap_or(0);

    let started = Instant::now();
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&repo).arg("maintenance").arg("run");
    for task in &effective_tasks {
        cmd.arg(format!("--task={task}"));
    }
    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            warn!("✗ {label}  failed to invoke git: {e}");
            bar.inc(1);
            items.lock().unwrap().push(TreeItem {
                label,
                detail: format!("failed to invoke git: {e}"),
                ok: false,
            });
            return;
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let size_after = dir_size(&gitdir).await.unwrap_or(size_before);

    let (ok, detail) = if output.status.success() {
        if size_after < size_before {
            total_freed.fetch_add(size_before - size_after, std::sync::atomic::Ordering::Relaxed);
        }
        let delta_str = size_delta_str(size_before, size_after);
        let bar_freed = total_freed.load(std::sync::atomic::Ordering::Relaxed);
        bar.set_message(format!("{} freed", format_size(bar_freed, BINARY)));
        info!("✓ {label}  {delta_str} in {elapsed_ms}ms");
        (true, format!("{delta_str} in {elapsed_ms}ms"))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let compact = stderr
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        warn!("✗ {label}  {compact} ({elapsed_ms}ms)");
        (false, format!("{compact} ({elapsed_ms}ms)"))
    };

    bar.inc(1);
    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

/// Format a before/after size pair as a human-readable delta.
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
        && let Some(path) = content.lines().next().and_then(|l| l.strip_prefix("gitdir: ")) {
            let p = std::path::Path::new(path);
            return if p.is_absolute() {
                PathBuf::from(p)
            } else {
                dotgit.parent().unwrap_or(std::path::Path::new(".")).join(p)
            };
        }
    dotgit
}

fn resolve_pack_dir(repo: &std::path::Path) -> PathBuf {
    resolve_gitdir(repo).join("objects/pack")
}

fn has_pack_files(pack_dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(pack_dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .map(|x| x == "pack")
            .unwrap_or(false)
    })
}
