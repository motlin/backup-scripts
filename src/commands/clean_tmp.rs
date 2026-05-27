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

use crate::config::{CleanTmpConfig, expand_tilde};
use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 2;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Paths to clean (defaults to /tmp). [config: clean_tmp.roots, default: /tmp]
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,

    /// Only delete files/dirs older than this many days. 0 = always clean. [config: clean_tmp.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_tmp.concurrency, default: 2]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanTmpConfig, dry_run: bool) -> Result<()> {
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let roots = if !args.roots.is_empty() {
        args.roots.iter().map(|p| expand_tilde(p)).collect()
    } else if let Some(cfg_roots) = &cfg.roots {
        cfg_roots.iter().map(|p| expand_tilde(p)).collect()
    } else {
        vec![PathBuf::from("/tmp")]
    };

    async move {
        let mut candidates: Vec<PathBuf> = Vec::new();

        for root in &roots {
            if !root.exists() {
                warn!("root does not exist: {}", root.display());
                continue;
            }
            // Walk the directory and find items older than the threshold
            for entry in WalkDir::new(root)
                .into_iter()
                .flatten()
                .filter(|e| e.path() != root)
            {
                if older_than_days(entry.path(), days) {
                    candidates.push(entry.path().to_path_buf());
                }
            }
        }

        candidates.sort();
        candidates.dedup();

        info!(found = candidates.len(), "old files/directories");

        if candidates.is_empty() {
            return Ok(());
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-tmp", candidates.len() as u64));

        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for path in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(path, dry_run, &total_bytes, &total_count, &bar, &items).await;
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
        ui::print_tree(&format!("clean-tmp: {summary}"), &items);

        Ok(())
    }
    .instrument(info_span!("clean-tmp", days,))
    .await
}

async fn clean_one(
    path: PathBuf,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
) {
    let label = path.display().to_string();

    let started = Instant::now();
    let size = if path.is_dir() {
        dir_size(&path).await.unwrap_or(0)
    } else {
        std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0)
    };

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let det = format!("would delete {}", format_size(size, BINARY));
        info!("✓ {label}  {det}");
        (true, det)
    } else {
        let result = if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await.map(|_| ())
        } else {
            tokio::fs::remove_file(&path).await.map(|_| ())
        };

        match result {
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
