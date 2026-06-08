use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use super::executor::{delete_dir, relative_label, run_parallel};
use crate::config::{CleanXcodeConfig, expand_tilde};
use crate::walk::older_than_days;

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to Xcode `DerivedData`. [config: `clean_xcode.data_dir`, default: ~/Library/Developer/Xcode/DerivedData]
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Only delete project dirs older than this many days. 0 = always clean. [config: `clean_xcode.days`, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: `clean_xcode.concurrency`, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanXcodeConfig, dry_run: bool) -> Result<CommandSummary> {
    let data_dir = args
        .data_dir
        .or_else(|| cfg.data_dir.clone())
        .map_or_else(default_derived_data, |p| expand_tilde(&p));
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    async move {
        if !data_dir.exists() {
            info!(
                "Xcode DerivedData does not exist (non-Mac or no Xcode), skipping: {}",
                data_dir.display()
            );
            return Ok(CommandSummary::default());
        }

        let project_dirs = find_project_dirs(&data_dir);
        info!(found = project_dirs.len(), "candidate project dirs");

        let candidates: Vec<PathBuf> = project_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let data_for_labels = Arc::new(data_dir.clone());
        let summary = run_parallel(
            "clean-xcode",
            candidates,
            concurrency,
            dry_run,
            move |dir, progress| {
                let data_for_labels = Arc::clone(&data_for_labels);
                async move {
                    let label = relative_label(&dir, &data_for_labels);
                    delete_dir(&dir, label, &progress).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-xcode", days))
    .await
}

/// Each entry directly under `DerivedData` is one project's build cache, named like
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
        let Ok(file_type) = entry.file_type() else {
            continue;
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
