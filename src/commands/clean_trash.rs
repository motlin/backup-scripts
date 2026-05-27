use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::fs;
use tracing::{Instrument, info, info_span, warn};

use crate::config::CleanTrashConfig;
use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::dir_size;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, _cfg: &CleanTrashConfig, dry_run: bool) -> Result<()> {
    let trash = default_trash_dir();

    let result = async move {
        if !trash.exists() {
            info!(path = %trash.display(), "Trash directory does not exist; skipping");
            return Ok::<_, anyhow::Error>(None);
        }

        let entries = match read_trash_entries(&trash) {
            Ok(e) => e,
            Err(e) => {
                warn!("cannot read {}: {e}", trash.display());
                return Ok::<_, anyhow::Error>(None);
            }
        };
        info!(found = entries.len(), "Trash entries");

        if entries.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let size_before = dir_size(&trash).await.unwrap_or(0);
        let started = Instant::now();

        let bar = CommandBar::new("clean-trash", entries.len() as u64);
        let total_bytes = AtomicU64::new(0);
        let total_count = AtomicU64::new(0);
        let items: Mutex<Vec<TreeItem>> = Mutex::new(Vec::new());

        // Process serially. Concurrent rm operations on the same parent directory
        // do not parallelize well, and the per-entry filesystem operations are quick.
        for entry in entries {
            clean_one(
                entry,
                dry_run,
                &total_bytes,
                &total_count,
                &bar,
                &items,
                &trash,
            )
            .await;
        }

        let size_after = if dry_run {
            size_before
        } else {
            dir_size(&trash).await.unwrap_or(0)
        };
        let freed = size_before.saturating_sub(size_after);

        let count = total_count.load(Ordering::Relaxed);
        let verb = if dry_run { "would free" } else { "freed" };
        let measured = if dry_run {
            total_bytes.load(Ordering::Relaxed)
        } else {
            freed
        };
        let summary = format!(
            "{verb} {} across {count} items in {}ms",
            format_size(measured, BINARY),
            started.elapsed().as_millis()
        );

        bar.finish_ok(summary.clone());

        let items = items.into_inner().unwrap_or_default();
        Ok::<_, anyhow::Error>(Some((summary, items)))
    }
    .instrument(info_span!("clean-trash"))
    .await?;

    if let Some((summary, items)) = result {
        ui::print_tree(&format!("clean-trash: {summary}"), &items);
    }
    Ok(())
}

/// Read direct entries of the Trash directory, skipping `.DS_Store` which macOS
/// regenerates anyway. We do NOT delete the Trash directory itself — only its contents.
fn read_trash_entries(trash: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(trash)?.flatten() {
        let name = entry.file_name();
        if name == ".DS_Store" {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

async fn clean_one(
    path: PathBuf,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    trash: &Path,
) {
    let label = path
        .strip_prefix(trash)
        .unwrap_or(&path)
        .display()
        .to_string();

    let started = Instant::now();
    let is_dir = path.is_dir();
    let size = if is_dir {
        dir_size(&path).await.unwrap_or(0)
    } else {
        std::fs::symlink_metadata(&path)
            .ok()
            .map(|m| m.len())
            .unwrap_or(0)
    };

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let det = format!("would delete {}", format_size(size, BINARY));
        info!("✓ {label}  {det}");
        (true, det)
    } else {
        let res = if is_dir {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        match res {
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
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

fn default_trash_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".Trash")
}
