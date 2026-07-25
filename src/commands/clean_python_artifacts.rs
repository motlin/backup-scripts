use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::executor::{delete_dir, relative_label, run_parallel};
use super::{CommandSummary, cleaner};
use crate::config::{CleanPythonArtifactsConfig, resolve_roots};
use crate::walk::older_than_days;

pub const DEFAULT_DEPTH: usize = 8;
pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to scan for Python artifacts. [config:
    /// `clean_python_artifacts.roots` or roots]
    #[arg(long = "root")]
    roots: Vec<PathBuf>,

    /// Maximum scan depth. [config: `clean_python_artifacts.depth`, default: 8]
    #[arg(long)]
    depth: Option<usize>,

    /// Only select artifact directories older than this many days.
    /// [config: `clean_python_artifacts.days`, default: 30]
    #[arg(long)]
    days: Option<u32>,

    /// Maximum number of parallel deletions.
    /// [config: `clean_python_artifacts.concurrency`, default: 4]
    #[arg(long)]
    concurrency: Option<usize>,

    /// Delete eligible `.venv` directories. Without this flag they are reported
    /// but retained. [config: `clean_python_artifacts.remove_virtualenvs`, default: false]
    #[arg(long)]
    remove_virtualenvs: bool,
}

pub async fn run(
    args: Args,
    cfg: &CleanPythonArtifactsConfig,
    global_roots: Option<&Vec<PathBuf>>,
    dry_run: bool,
) -> Result<CommandSummary> {
    let roots = resolve_roots(
        &args.roots,
        cfg.roots.as_ref(),
        global_roots,
        "clean_python_artifacts",
    )?;
    let depth = args.depth.or(cfg.depth).unwrap_or(DEFAULT_DEPTH);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let remove_virtualenvs = args.remove_virtualenvs || cfg.remove_virtualenvs.unwrap_or(false);

    async move {
        let artifacts = discover_artifacts(&roots, depth, days);
        info!(
            bytecode = artifacts.bytecode.len(),
            virtualenvs = artifacts.virtualenvs.len(),
            "Python artifact candidates"
        );

        let bytecode = verify_candidates(artifacts.bytecode).await?;
        let virtualenvs = verify_candidates(artifacts.virtualenvs).await?;
        let mut summary = delete_candidates(
            "clean-python-bytecode",
            bytecode,
            &roots,
            concurrency,
            dry_run,
        )
        .await;

        if remove_virtualenvs || dry_run {
            summary.merge(
                delete_candidates(
                    "clean-python-virtualenvs",
                    virtualenvs,
                    &roots,
                    concurrency,
                    dry_run,
                )
                .await,
            );
        } else {
            for virtualenv in virtualenvs {
                info!(
                    path = %virtualenv.display(),
                    "stale virtual environment retained; enable --remove-virtualenvs"
                );
            }
        }

        Ok(summary)
    }
    .instrument(info_span!(
        "clean-python-artifacts",
        days,
        remove_virtualenvs
    ))
    .await
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Artifacts {
    bytecode: Vec<PathBuf>,
    virtualenvs: Vec<PathBuf>,
}

fn discover_artifacts(roots: &[PathBuf], depth: usize, days: u32) -> Artifacts {
    let mut artifacts = Artifacts::default();
    for root in roots {
        if !root.exists() {
            warn!("root does not exist: {}", root.display());
            continue;
        }
        let mut entries = WalkDir::new(root)
            .max_depth(depth)
            .follow_links(false)
            .into_iter();
        while let Some(result) = entries.next() {
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(%error, "could not inspect Python artifact path");
                    continue;
                }
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            match entry.file_name().to_str() {
                Some("__pycache__") => {
                    if older_than_days(path, days) {
                        artifacts.bytecode.push(path.to_path_buf());
                    }
                    entries.skip_current_dir();
                }
                Some(".venv") => {
                    if older_than_days(path, days) {
                        artifacts.virtualenvs.push(path.to_path_buf());
                    }
                    entries.skip_current_dir();
                }
                Some(".git" | "build" | "dist" | "node_modules" | "target") => {
                    entries.skip_current_dir();
                }
                _ => {}
            }
        }
    }
    artifacts.bytecode.sort();
    artifacts.bytecode.dedup();
    artifacts.virtualenvs.sort();
    artifacts.virtualenvs.dedup();
    artifacts
}

async fn verify_candidates(candidates: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut verified = Vec::new();
    for candidate in candidates {
        if cleaner::git_allows_path_deletion(&candidate).await? {
            verified.push(candidate);
        }
    }
    Ok(verified)
}

async fn delete_candidates(
    label: &'static str,
    candidates: Vec<PathBuf>,
    roots: &[PathBuf],
    concurrency: usize,
    dry_run: bool,
) -> CommandSummary {
    let labeled = candidates
        .into_iter()
        .map(|path| {
            let display = roots
                .iter()
                .find(|root| path.starts_with(root))
                .map_or_else(
                    || path.display().to_string(),
                    |root| relative_label(&path, root),
                );
            (path, display)
        })
        .collect();
    run_parallel(
        label,
        labeled,
        concurrency,
        dry_run,
        |(path, display), progress| async move {
            delete_dir(&path, display, &progress).await;
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_artifacts_without_descending_into_dependency_trees() {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let root = directory.path();
        let bytecode = root.join("alice/package/__pycache__");
        let virtualenv = root.join("alice/.venv");
        let skipped = virtualenv.join("lib/python/site-packages/__pycache__");
        let target = root.join("bob/target/__pycache__");
        std::fs::create_dir_all(&bytecode).expect("bytecode directory is created");
        std::fs::create_dir_all(&skipped).expect("virtualenv bytecode is created");
        std::fs::create_dir_all(&target).expect("target bytecode is created");

        assert_eq!(
            discover_artifacts(&[root.to_path_buf()], 8, 0),
            Artifacts {
                bytecode: vec![bytecode],
                virtualenvs: vec![virtualenv],
            }
        );
    }

    #[test]
    fn scan_depth_limits_nested_artifacts() {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let root = directory.path();
        std::fs::create_dir_all(root.join("alice/package/__pycache__"))
            .expect("nested bytecode directory is created");

        assert_eq!(
            discover_artifacts(&[root.to_path_buf()], 2, 0),
            Artifacts::default()
        );
    }
}
