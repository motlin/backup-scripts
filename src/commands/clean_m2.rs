use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use crate::config::{CleanM2Config, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 60;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_MARKER_EXTENSION: &str = "pom";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the Maven local repository. [config: clean_m2.repo, default: ~/.m2/repository]
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Only delete version directories older than this many days. 0 = always clean. [config: clean_m2.days, default: 60]
    #[arg(long)]
    days: Option<u32>,

    /// Restrict to SNAPSHOT versions only (directories ending in `-SNAPSHOT`). [config: clean_m2.snapshots_only, default: false]
    #[arg(long)]
    snapshots_only: bool,

    /// Maximum number of parallel deletions. [config: clean_m2.concurrency, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,

    /// File extension used to identify version directories. [config: clean_m2.marker_extension, default: pom]
    #[arg(long)]
    marker_extension: Option<String>,
}

pub async fn run(args: Args, cfg: &CleanM2Config, dry_run: bool) -> Result<()> {
    let repo = args
        .repo
        .or_else(|| cfg.repo.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_m2_repo);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let snapshots_only = args.snapshots_only || cfg.snapshots_only.unwrap_or(false);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let marker_extension = args
        .marker_extension
        .or_else(|| cfg.marker_extension.clone())
        .unwrap_or_else(|| DEFAULT_MARKER_EXTENSION.to_string());

    async move {
        if !repo.exists() {
            warn!("m2 repository does not exist: {}", repo.display());
            return Ok(());
        }

        let version_dirs = find_version_dirs(&repo, snapshots_only, &marker_extension);
        info!(found = version_dirs.len(), "candidate version dirs");

        let candidates: Vec<PathBuf> = version_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        if candidates.is_empty() {
            return Ok(());
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-m2", candidates.len() as u64));

        let repo_for_labels = Arc::new(repo.clone());
        let max_label = candidates
            .iter()
            .map(|d| dir_label(d, &repo_for_labels).chars().count())
            .max()
            .unwrap_or(0);
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let repo_for_labels = Arc::clone(&repo_for_labels);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        dir,
                        max_label,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                        &repo_for_labels,
                    )
                    .await;
                }
                .in_current_span(),
            );
        }
        while set.join_next().await.is_some() {}

        let count = total_count.load(Ordering::Relaxed);
        let bytes = total_bytes.load(Ordering::Relaxed);
        let verb = if dry_run { "would free" } else { "freed" };
        let summary = format!("{verb} {} across {count} items", format_size(bytes, BINARY));

        let bar = Arc::try_unwrap(bar).unwrap_or_else(|_| panic!("bar arc leaked"));
        bar.finish_ok(summary.clone());

        let items = Arc::try_unwrap(items)
            .unwrap_or_else(|_| panic!("items arc leaked"))
            .into_inner()
            .unwrap_or_default();
        ui::print_tree(&format!("clean-m2: {summary}"), &items);

        Ok(())
    }
    .instrument(info_span!("clean-m2", snapshots_only, days,))
    .await
}

fn dir_label(dir: &Path, m2_repo: &Path) -> String {
    dir.strip_prefix(m2_repo)
        .unwrap_or(dir)
        .display()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn clean_one(
    dir: PathBuf,
    max_label: usize,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    m2_repo: &Path,
) {
    let label = dir_label(&dir, m2_repo);
    let padded = pad_right(&label, max_label);

    let started = Instant::now();
    let size = dir_size(&dir).await.unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        info!("✓ {padded}  {}", ui::format_detail(&detail));
        (true, detail)
    } else {
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let detail = ItemDetail::success(
                    "deleted",
                    format_size(size, BINARY),
                    format_duration(started.elapsed().as_millis() as u64),
                );
                info!("✓ {padded}  {}", ui::format_detail(&detail));
                (true, detail)
            }
            Err(e) => {
                let detail = ItemDetail::failure(format!("{e}"));
                warn!("✗ {padded}  {}", ui::format_detail(&detail));
                (false, detail)
            }
        }
    };

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

/// A version directory is one that contains at least one top-level `*.<marker_extension>` file.
fn find_version_dirs(repo: &Path, snapshots_only: bool, marker_extension: &str) -> Vec<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let follow = crate::walk::follow_symlinks();
    for entry in WalkDir::new(repo)
        .follow_links(follow)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|e| e.to_str()) != Some(marker_extension) {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        if snapshots_only {
            let name = parent
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !name.ends_with("-SNAPSHOT") {
                continue;
            }
        }
        dirs.insert(parent.to_path_buf());
    }
    dirs.into_iter().collect()
}

fn default_m2_repo() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".m2/repository")
}
