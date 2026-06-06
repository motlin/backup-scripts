use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tracing::{Instrument, info, info_span, warn};
use walkdir::WalkDir;

use super::CommandSummary;
use super::executor::{CleanProgress, relative_label, run_parallel};
use crate::config::{CleanLogsConfig, expand_tilde};
use crate::ui::{self, ItemDetail, format_duration};
use crate::walk::older_than_days;

pub const DEFAULT_DAYS: u32 = 30;
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Path component that is never descended into. `DiagnosticReports/` holds `.ips`
/// crash reports that AppleCare/Apple support ask for; macOS already auto-purges
/// them after a few days, so we leave them alone.
const EXCLUDED_COMPONENT: &str = "DiagnosticReports";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to the logs directory. [config: clean_logs.cache_dir, default: ~/Library/Logs]
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Only delete log files older than this many days. 0 = always clean. [config: clean_logs.days, default: 30]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_logs.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(args: Args, cfg: &CleanLogsConfig, dry_run: bool) -> Result<CommandSummary> {
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
            info!("logs dir does not exist, skipping: {}", cache_dir.display());
            return Ok(CommandSummary::default());
        }

        let files = find_log_files(&cache_dir);
        info!(found = files.len(), "candidate log files");

        let candidates: Vec<PathBuf> = files
            .into_iter()
            .filter(|f| older_than_days(f, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let cache_for_labels = Arc::new(cache_dir.clone());
        let summary = run_parallel(
            "clean-logs",
            candidates,
            concurrency,
            dry_run,
            move |file, progress| {
                let cache_for_labels = Arc::clone(&cache_for_labels);
                async move {
                    clean_one(file, &progress, &cache_for_labels).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-logs", days))
    .await
}

async fn clean_one(file: PathBuf, progress: &CleanProgress, cache_dir: &Path) {
    let label = relative_label(&file, cache_dir);

    let started = Instant::now();
    let size = tokio::fs::metadata(&file)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    if progress.dry_run() {
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        progress.record(label, detail, true, size);
        return;
    }

    match tokio::fs::remove_file(&file).await {
        Ok(()) => {
            let detail = ItemDetail::success(
                "deleted",
                format_size(size, BINARY),
                format_duration(started.elapsed().as_millis() as u64),
            );
            progress.record(label, detail, true, size);
        }
        // A log a process is actively writing can fail with EBUSY or a
        // permission error. Record the failure and move on — never abort the
        // whole run over one locked file.
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            warn!("✗ {label}  {}", ui::format_detail(&detail));
            progress.record(label, detail, false, 0);
        }
    }
}

/// True when any path component (relative to `cache_dir`) is excluded. We keep
/// `DiagnosticReports/` and everything under it untouched.
fn is_excluded(file: &Path, cache_dir: &Path) -> bool {
    let rel = file.strip_prefix(cache_dir).unwrap_or(file);
    rel.components()
        .any(|c| c.as_os_str() == EXCLUDED_COMPONENT)
}

/// Recursively collect every regular file under `cache_dir`, keeping the
/// directory tree intact (only files are returned, so apps keep their log
/// folders and can keep writing). Files under any `DiagnosticReports/` segment
/// are skipped.
fn find_log_files(cache_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(cache_dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if is_excluded(path, cache_dir) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    files
}

fn default_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, self-cleaning fixture root under the OS temp dir.
    fn fixture_root(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("backup-clean-logs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn find_log_files_collects_nested_files_keeps_dirs() {
        let root = fixture_root("nested");
        std::fs::create_dir_all(root.join("Claude")).unwrap();
        std::fs::write(root.join("Claude/app.log"), b"x").unwrap();
        std::fs::write(root.join("top.log"), b"x").unwrap();

        let found = find_log_files(&root);

        assert_eq!(
            found,
            vec![root.join("Claude/app.log"), root.join("top.log")],
            "returns regular files at every depth, never the directories themselves"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_log_files_skips_diagnostic_reports() {
        let root = fixture_root("diag");
        std::fs::create_dir_all(root.join("DiagnosticReports")).unwrap();
        std::fs::write(root.join("DiagnosticReports/crash.ips"), b"x").unwrap();
        std::fs::create_dir_all(root.join("Sub/DiagnosticReports")).unwrap();
        std::fs::write(root.join("Sub/DiagnosticReports/nested.ips"), b"x").unwrap();
        std::fs::write(root.join("keep.log"), b"x").unwrap();

        let found = find_log_files(&root);

        assert_eq!(
            found,
            vec![root.join("keep.log")],
            "DiagnosticReports at any depth is excluded; .ips crash reports are kept"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn is_excluded_matches_only_diagnostic_reports_component() {
        let root = Path::new("/Users/me/Library/Logs");
        assert!(is_excluded(&root.join("DiagnosticReports/crash.ips"), root));
        assert!(is_excluded(
            &root.join("App/DiagnosticReports/crash.ips"),
            root
        ));
        assert!(!is_excluded(&root.join("App/app.log"), root));
        assert!(
            !is_excluded(&root.join("DiagnosticReportsBackup/x.log"), root),
            "only an exact component match is excluded, not a prefix"
        );
    }

    #[test]
    fn file_label_is_relative_to_cache_dir() {
        let cache = Path::new("/Users/me/Library/Logs");
        let file = cache.join("Claude/main.log");
        assert_eq!(relative_label(&file, cache), "Claude/main.log");
    }
}
