use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};

use crate::config::{CleanJetBrainsConfig, expand_tilde};
use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to JetBrains caches. [config: clean_jetbrains.cache_dir, default: ~/Library/Caches/JetBrains]
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Only delete per-product dirs older than this many days. 0 = always clean. [config: clean_jetbrains.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_jetbrains.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanJetBrainsConfig, dry_run: bool) -> Result<()> {
    let cache_dir = args
        .cache_dir
        .or_else(|| cfg.cache_dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_cache_dir);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let result = async move {
        if !cache_dir.exists() {
            info!(
                "JetBrains caches dir does not exist, skipping: {}",
                cache_dir.display()
            );
            return Ok::<_, anyhow::Error>(None);
        }

        let product_dirs = find_product_dirs(&cache_dir);
        info!(found = product_dirs.len(), "candidate per-product dirs");

        let candidates: Vec<PathBuf> = product_dirs
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
        let bar = Arc::new(CommandBar::new("clean-jetbrains", candidates.len() as u64));

        let cache_for_labels = Arc::new(cache_dir.clone());
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let cache_for_labels = Arc::clone(&cache_for_labels);
            let label = dir
                .strip_prefix(&cache_dir)
                .unwrap_or(&dir)
                .display()
                .to_string();
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        dir,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                        &cache_for_labels,
                    )
                    .await;
                }
                .instrument(info_span!("product", name = %label)),
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
    .instrument(info_span!("clean-jetbrains", days))
    .await?;

    if let Some((summary, items)) = result {
        ui::print_tree(&format!("clean-jetbrains: {summary}"), &items);
    }
    Ok(())
}

async fn clean_one(
    dir: PathBuf,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    cache_dir: &Path,
) {
    let label = dir
        .strip_prefix(cache_dir)
        .unwrap_or(&dir)
        .display()
        .to_string();

    let started = Instant::now();
    let size = dir_size(&dir).await.unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let det = format!("would delete {}", format_size(size, BINARY));
        info!("✓ {label}  {det}");
        (true, det)
    } else {
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let det = format!(
                    "deleted {} in {}ms",
                    format_size(size, BINARY),
                    started.elapsed().as_millis()
                );
                info!("✓ {label}  {det}");
                (true, det)
            }
            Err(e) => {
                let det = format!("failed: {e}");
                warn!("✗ {label}  {det}");
                (false, det)
            }
        }
    };

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

/// Top-level entries under ~/Library/Caches/JetBrains are per-product caches
/// (`IntelliJIdea2026.1`, `WebStorm2026.1`, …) plus the `Toolbox` directory.
/// `Toolbox` is the installed-IDE store, not a cache, so skip it.
fn find_product_dirs(cache_dir: &Path) -> Vec<PathBuf> {
    let read_dir = match std::fs::read_dir(cache_dir) {
        Ok(rd) => rd,
        Err(e) => {
            warn!("cannot read {}: {e}", cache_dir.display());
            return Vec::new();
        }
    };

    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in read_dir.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if is_excluded_child(name.to_string_lossy().as_ref()) {
            continue;
        }
        dirs.push(entry.path());
    }
    dirs.sort();
    dirs
}

fn is_excluded_child(name: &str) -> bool {
    name == "Toolbox"
}

fn default_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Caches/JetBrains")
}
