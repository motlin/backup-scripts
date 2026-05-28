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

use super::CommandSummary;
use crate::config::{CleanXcodeConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to Xcode DerivedData. [config: clean_xcode.data_dir, default: ~/Library/Developer/Xcode/DerivedData]
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Only delete project dirs older than this many days. 0 = always clean. [config: clean_xcode.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_xcode.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanXcodeConfig, dry_run: bool) -> Result<CommandSummary> {
    let data_dir = args
        .data_dir
        .or_else(|| cfg.data_dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_derived_data);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let result = async move {
        if !data_dir.exists() {
            info!(
                "Xcode DerivedData does not exist (non-Mac or no Xcode), skipping: {}",
                data_dir.display()
            );
            return Ok::<_, anyhow::Error>(None);
        }

        let project_dirs = find_project_dirs(&data_dir);
        info!(found = project_dirs.len(), "candidate project dirs");

        let candidates: Vec<PathBuf> = project_dirs
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
        let bar = Arc::new(CommandBar::new("clean-xcode", candidates.len() as u64));

        let data_for_labels = Arc::new(data_dir.clone());
        let max_label = candidates
            .iter()
            .map(|d| dir_label(d, &data_dir).chars().count())
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
            let data_for_labels = Arc::clone(&data_for_labels);
            let label = dir_label(&dir, &data_dir);
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
                        &data_for_labels,
                    )
                    .await;
                }
                .instrument(info_span!("project", name = %label)),
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

        Ok::<_, anyhow::Error>(Some((summary, items, bytes)))
    }
    .instrument(info_span!("clean-xcode", days))
    .await?;

    if let Some((summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::print_tree(&format!("clean-xcode: {summary}"), &items);
        Ok(CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
        })
    } else {
        Ok(CommandSummary::default())
    }
}

fn dir_label(dir: &Path, data_dir: &Path) -> String {
    dir.strip_prefix(data_dir)
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
    data_dir: &Path,
) {
    let label = dir_label(&dir, data_dir);
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

/// Each entry directly under DerivedData is one project's build cache, named like
/// `<project-name>-<hash>`. We only consider TOP-LEVEL directory entries — we do not
/// recurse — so a single mtime check on the project dir decides eligibility. This is
/// fast and aligns with how Xcode regenerates these caches on the next build.
fn find_project_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let read_dir = match std::fs::read_dir(data_dir) {
        Ok(rd) => rd,
        Err(e) => {
            warn!("cannot read {}: {e}", data_dir.display());
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

fn default_derived_data() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Developer/Xcode/DerivedData")
}
