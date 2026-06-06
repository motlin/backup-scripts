use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::CommandSummary;
use super::executor::{CleanProgress, relative_label, run_parallel};
use crate::config::{CleanNpmConfig, expand_tilde};
use crate::ui::{self, ItemDetail, format_duration};
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

pub async fn run(args: Args, cfg: &CleanNpmConfig, dry_run: bool) -> Result<CommandSummary> {
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

    async move {
        if !cache_dir.exists() {
            // Skip silently if no npm cache present (e.g. npm not installed).
            info!(path = %cache_dir.display(), "npm cache does not exist; skipping");
            return Ok(CommandSummary::default());
        }

        let content_dir = cache_dir.join("content-v2");
        if !content_dir.exists() {
            info!(path = %content_dir.display(), "npm content-v2 dir does not exist; skipping");
            return Ok(CommandSummary::default());
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

        let cache_for_labels = Arc::new(cache_dir.clone());
        let summary = run_parallel(
            "clean-npm",
            candidates,
            concurrency,
            dry_run,
            move |path, progress| {
                let cache_for_labels = Arc::clone(&cache_for_labels);
                async move {
                    clean_one(path, &progress, &cache_for_labels).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-npm", days))
    .await
}

async fn clean_one(path: PathBuf, progress: &CleanProgress, cache_dir: &std::path::Path) {
    let label = relative_label(&path, cache_dir);

    let started = Instant::now();
    let size = std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);

    if progress.dry_run() {
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        progress.record(label, detail, true, size);
        return;
    }

    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            let detail = ItemDetail::success(
                "deleted",
                format_size(size, BINARY),
                format_duration(started.elapsed().as_millis() as u64),
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

fn default_npm_cache() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".npm/_cacache")
}
