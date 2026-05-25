use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use crate::config::{CleanNpmConfig, expand_tilde};
use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::older_than_days;

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the npm cacache directory. [config: clean_npm.cache_dir, default: ~/.npm/_cacache]
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Only delete cache entries older than this many days. 0 = always clean. [config: clean_npm.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_npm.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanNpmConfig, dry_run: bool) -> Result<()> {
    let cache_dir = args
        .cache_dir
        .or_else(|| cfg.cache_dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_npm_cache);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let result = async move {
        if !cache_dir.exists() {
            // Skip silently if no npm cache present (e.g. npm not installed).
            info!(path = %cache_dir.display(), "npm cache does not exist; skipping");
            return Ok::<_, anyhow::Error>(None);
        }

        let content_dir = cache_dir.join("content-v2");
        if !content_dir.exists() {
            info!(path = %content_dir.display(), "npm content-v2 dir does not exist; skipping");
            return Ok::<_, anyhow::Error>(None);
        }

        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(&content_dir).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            if older_than_days(entry.path(), days) {
                candidates.push(entry.path().to_path_buf());
            }
        }

        candidates.sort();
        candidates.dedup();

        info!(found = candidates.len(), "old cache entries");

        if candidates.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-npm", candidates.len() as u64));

        let cache_for_labels = Arc::new(cache_dir.clone());
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for path in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let cache_for_labels = Arc::clone(&cache_for_labels);
            let label = path
                .strip_prefix(cache_for_labels.as_path())
                .unwrap_or(&path)
                .display()
                .to_string();
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        path,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                        &cache_for_labels,
                    )
                    .await;
                }
                .instrument(info_span!("entry", name = %label)),
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
    .instrument(info_span!("clean-npm", days))
    .await?;

    if let Some((summary, items)) = result {
        ui::print_tree(&format!("clean-npm: {summary}"), &items);
    }
    Ok(())
}

async fn clean_one(
    path: PathBuf,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    cache_dir: &std::path::Path,
) {
    let label = path
        .strip_prefix(cache_dir)
        .unwrap_or(&path)
        .display()
        .to_string();

    let started = Instant::now();
    let size = std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let det = format!("would delete {}", format_size(size, BINARY));
        info!("✓ {label}  {det}");
        (true, det)
    } else {
        match tokio::fs::remove_file(&path).await {
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
    bar.set_message(format!("{} freed", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

fn default_npm_cache() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".npm/_cacache")
}
