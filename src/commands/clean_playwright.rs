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

use crate::config::{CleanPlaywrightConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to Playwright browsers cache. [config: clean_playwright.cache_dir, default: ~/Library/Caches/ms-playwright]
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Only delete browser-version dirs older than this many days. 0 = always clean. [config: clean_playwright.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_playwright.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanPlaywrightConfig, dry_run: bool) -> Result<()> {
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
                "Playwright cache dir does not exist, skipping: {}",
                cache_dir.display()
            );
            return Ok::<_, anyhow::Error>(None);
        }

        let version_dirs = find_version_dirs(&cache_dir);
        info!(found = version_dirs.len(), "candidate browser-version dirs");

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
        let bar = Arc::new(CommandBar::new("clean-playwright", candidates.len() as u64));

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
                .instrument(info_span!("browser", name = %label)),
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
    .instrument(info_span!("clean-playwright", days))
    .await?;

    if let Some((summary, items)) = result {
        ui::print_tree(&format!("clean-playwright: {summary}"), &items);
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

/// Entries directly under ~/Library/Caches/ms-playwright are per-browser-version
/// dirs (`chromium-1217`, `chromium_headless_shell-1217`, `ffmpeg-1011`, …).
fn find_version_dirs(cache_dir: &Path) -> Vec<PathBuf> {
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
        dirs.push(entry.path());
    }
    dirs.sort();
    dirs
}

fn default_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Caches/ms-playwright")
}
