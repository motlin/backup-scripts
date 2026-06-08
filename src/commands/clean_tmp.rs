use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::time::Instant;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::CommandSummary;
use super::executor::{CleanProgress, run_parallel};
use crate::config::{CleanTmpConfig, expand_tilde};
use crate::ui::{self, ItemDetail, format_duration};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 2;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Paths to clean (defaults to /tmp). [config: `clean_tmp.roots`, default: /tmp]
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,

    /// Only delete files/dirs older than this many days. 0 = always clean. [config: `clean_tmp.days`, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: `clean_tmp.concurrency`, default: 2]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanTmpConfig, dry_run: bool) -> Result<CommandSummary> {
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

        let summary = run_parallel(
            "clean-tmp",
            candidates,
            concurrency,
            dry_run,
            move |path, progress| async move {
                clean_one(path, &progress).await;
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-tmp", days,))
    .await
}

async fn clean_one(path: PathBuf, progress: &CleanProgress) {
    let label = path.display().to_string();

    let started = Instant::now();
    let size = if path.is_dir() {
        dir_size(&path).await.unwrap_or(0)
    } else {
        std::fs::metadata(&path).ok().map_or(0, |m| m.len())
    };

    if progress.dry_run() {
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        progress.record(label, detail, true, size);
        return;
    }

    let result = if path.is_dir() {
        tokio::fs::remove_dir_all(&path).await
    } else {
        tokio::fs::remove_file(&path).await
    };

    match result {
        Ok(()) => {
            let detail = ItemDetail::success(
                "deleted",
                format_size(size, BINARY),
                format_duration(started.elapsed().as_millis()),
            );
            progress.record(label, detail, true, size);
        }
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            warn!("✗ {label}  {}", ui::format_detail(&detail));
            progress.record(label, detail, false, 0);
        }
    }
}
