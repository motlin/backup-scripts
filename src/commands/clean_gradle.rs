use anyhow::Result;
use clap::Args as ClapArgs;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::CommandSummary;
use super::executor::{delete_dir, relative_label, run_parallel};
use crate::config::{CleanGradleConfig, expand_tilde};
use crate::walk::older_than_days;

pub const DEFAULT_DAYS: u32 = 60;
pub const DEFAULT_CONCURRENCY: usize = 8;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the Gradle modules cache. [config: `clean_gradle.cache_dir`, default: ~/.gradle/caches/modules-2/files-2.1]
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Only delete version directories older than this many days. 0 = always clean. [config: `clean_gradle.days`, default: 60]
    #[arg(long)]
    days: Option<u32>,

    /// Maximum number of parallel deletions. [config: `clean_gradle.concurrency`, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanGradleConfig, dry_run: bool) -> Result<CommandSummary> {
    let cache_dir = args
        .cache_dir
        .or_else(|| cfg.cache_dir.clone())
        .map_or_else(default_gradle_cache, |p| expand_tilde(&p));
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    async move {
        if !cache_dir.exists() {
            warn!("gradle cache does not exist: {}", cache_dir.display());
            return Ok(CommandSummary::default());
        }

        let version_dirs = find_version_dirs(&cache_dir);
        info!(found = version_dirs.len(), "candidate version dirs");

        let candidates: Vec<PathBuf> = version_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let cache_for_labels = Arc::new(cache_dir.clone());
        let summary = run_parallel(
            "clean-gradle",
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
    .instrument(info_span!("clean-gradle", days))
    .await
}

/// A Gradle version directory lives at `<cache>/<group>/<artifact>/<version>/` and contains
/// hash subdirectories whose children are files (the cached jars/poms/etc.).
///
/// We identify version dirs structurally: walk the cache and for every regular file at
/// depth 5 (cache=0, group=1, artifact=2, version=3, hash=4, file=5), record the
/// grandparent (the version dir). This mirrors `clean_m2`'s approach of deriving version
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
