use anyhow::Result;
use clap::Args as ClapArgs;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::CommandSummary;
use super::executor::{delete_dir, relative_label, run_parallel};
use crate::config::{CleanM2Config, expand_tilde};
use crate::walk::older_than_days;

pub const DEFAULT_DAYS: u32 = 60;
pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_MARKER_EXTENSION: &str = "pom";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the Maven local repository. [config: clean_m2.repo, default: ~/.m2/repository]
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Only delete version directories older than this many days. 0 = always clean. [config: clean_m2.days, default: 60]
    #[arg(long)]
    days: Option<u32>,

    /// Restrict to SNAPSHOT versions only (directories ending in `-SNAPSHOT`). [config: clean_m2.snapshots_only, default: false]
    #[arg(long)]
    snapshots_only: bool,

    /// Maximum number of parallel deletions. [config: clean_m2.concurrency, default: 8]
    #[arg(long)]
    concurrency: Option<usize>,

    /// File extension used to identify version directories. [config: clean_m2.marker_extension, default: pom]
    #[arg(long)]
    marker_extension: Option<String>,
}

pub async fn run(args: Args, cfg: &CleanM2Config, dry_run: bool) -> Result<CommandSummary> {
    let repo = args
        .repo
        .or_else(|| cfg.repo.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_m2_repo);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let snapshots_only = args.snapshots_only || cfg.snapshots_only.unwrap_or(false);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let marker_extension = args
        .marker_extension
        .or_else(|| cfg.marker_extension.clone())
        .unwrap_or_else(|| DEFAULT_MARKER_EXTENSION.to_string());

    async move {
        if !repo.exists() {
            warn!("m2 repository does not exist: {}", repo.display());
            return Ok(CommandSummary::default());
        }

        let version_dirs = find_version_dirs(&repo, snapshots_only, &marker_extension);
        info!(found = version_dirs.len(), "candidate version dirs");

        let candidates: Vec<PathBuf> = version_dirs
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let repo_for_labels = Arc::new(repo.clone());
        let summary = run_parallel(
            "clean-m2",
            candidates,
            concurrency,
            dry_run,
            move |dir, progress| {
                let repo_for_labels = Arc::clone(&repo_for_labels);
                async move {
                    let label = relative_label(&dir, &repo_for_labels);
                    delete_dir(&dir, label, &progress).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-m2", snapshots_only, days,))
    .await
}

/// A version directory is one that contains at least one top-level `*.<marker_extension>` file.
fn find_version_dirs(repo: &Path, snapshots_only: bool, marker_extension: &str) -> Vec<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let follow = crate::walk::follow_symlinks();
    for entry in WalkDir::new(repo)
        .follow_links(follow)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|e| e.to_str()) != Some(marker_extension) {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        if snapshots_only {
            let name = parent
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !name.ends_with("-SNAPSHOT") {
                continue;
            }
        }
        dirs.insert(parent.to_path_buf());
    }
    dirs.into_iter().collect()
}

fn default_m2_repo() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".m2/repository")
}
