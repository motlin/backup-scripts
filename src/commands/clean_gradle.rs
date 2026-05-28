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

use crate::config::{CleanGradleConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 60;
pub const DEFAULT_CONCURRENCY: usize = 8;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the Gradle modules cache. [config: clean_gradle.cache_dir, default: ~/.gradle/caches/modules-2/files-2.1]
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Only delete version directories older than this many days. 0 = always clean. [config: clean_gradle.days, default: 60]
    #[arg(long)]
    days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_gradle.concurrency, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanGradleConfig, dry_run: bool) -> Result<()> {
    let cache_dir = args
        .cache_dir
        .or_else(|| cfg.cache_dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_gradle_cache);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let result = async move {
        if !cache_dir.exists() {
            warn!("gradle cache does not exist: {}", cache_dir.display());
            return Ok::<_, anyhow::Error>(None);
        }

        let version_dirs = find_version_dirs(&cache_dir);
        info!(found = version_dirs.len(), "candidate version dirs");

        let candidates: Vec<PathBuf> = version_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        if candidates.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-gradle", candidates.len() as u64));

        let cache_for_labels = Arc::new(cache_dir.clone());
        let max_label = candidates
            .iter()
            .map(|d| dir_label(d, &cache_dir).chars().count())
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
            let cache_for_labels = Arc::clone(&cache_for_labels);
            let label = dir_label(&dir, &cache_dir);
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
                        &cache_for_labels,
                    )
                    .await;
                }
                .instrument(info_span!("version", name = %label)),
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

        Ok::<_, anyhow::Error>(Some((summary, items)))
    }
    .instrument(info_span!("clean-gradle", days))
    .await?;

    if let Some((summary, items)) = result {
        ui::print_tree(&format!("clean-gradle: {summary}"), &items);
    }
    Ok(())
}

fn dir_label(dir: &Path, cache_dir: &Path) -> String {
    dir.strip_prefix(cache_dir)
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
    cache_dir: &Path,
) {
    let label = dir_label(&dir, cache_dir);
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

/// A Gradle version directory lives at `<cache>/<group>/<artifact>/<version>/` and contains
/// hash subdirectories whose children are files (the cached jars/poms/etc.).
///
/// We identify version dirs structurally: walk the cache and for every regular file at
/// depth 5 (cache=0, group=1, artifact=2, version=3, hash=4, file=5), record the
/// grandparent (the version dir). This mirrors clean_m2's approach of deriving version
/// dirs from the files they contain, but uses depth instead of a marker extension since
/// Gradle's cache stores each artifact's files under per-hash subdirectories.
fn find_version_dirs(cache_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let follow = crate::walk::follow_symlinks();
    for entry in WalkDir::new(cache_dir)
        .min_depth(5)
        .max_depth(5)
        .follow_links(follow)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(hash_dir) = entry.path().parent() else {
            continue;
        };
        let Some(version_dir) = hash_dir.parent() else {
            continue;
        };
        dirs.insert(version_dir.to_path_buf());
    }
    dirs.into_iter().collect()
}

fn default_gradle_cache() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".gradle/caches/modules-2/files-2.1")
}
