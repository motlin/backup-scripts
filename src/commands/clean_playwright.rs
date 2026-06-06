use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use super::executor::{delete_dir, relative_label, run_parallel};
use crate::config::{CleanPlaywrightConfig, expand_tilde};
use crate::walk::older_than_days;

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

pub async fn run(args: Args, cfg: &CleanPlaywrightConfig, dry_run: bool) -> Result<CommandSummary> {
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

    async move {
        if !cache_dir.exists() {
            info!(
                "Playwright cache dir does not exist, skipping: {}",
                cache_dir.display()
            );
            return Ok(CommandSummary::default());
        }

        let version_dirs = find_version_dirs(&cache_dir);
        info!(found = version_dirs.len(), "candidate browser-version dirs");

        let candidates: Vec<PathBuf> = version_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let cache_for_labels = Arc::new(cache_dir.clone());
        let summary = run_parallel(
            "clean-playwright",
            candidates,
            concurrency,
            dry_run,
            move |dir, progress| {
                let cache_for_labels = Arc::clone(&cache_for_labels);
                async move {
                    let label = relative_label(&dir, &cache_for_labels);
                    delete_dir(&dir, label, &progress).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-playwright", days))
    .await
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
