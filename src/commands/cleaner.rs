use anyhow::Result;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, warn};

use crate::ui::{self, CommandBar, TreeItem};
use crate::walk::{dir_size, find_dirs_with_marker, older_than_days};

/// Caller is responsible for wrapping this future in its own `info_span!` (e.g.
/// `info_span!("clean-maven")`) so the span name shows up in scrollback as the actual
/// command rather than a generic "cleaner".
#[allow(clippy::too_many_arguments)]
pub async fn clean(
    bar_label: &'static str,
    marker: &'static str,
    junk: &'static str,
    roots: Vec<PathBuf>,
    depth: usize,
    days: u32,
    concurrency: usize,
    dry_run: bool,
) -> Result<()> {
    let mut projects: Vec<PathBuf> = Vec::new();
    for root in &roots {
        if !root.exists() {
            warn!("root does not exist: {}", root.display());
            continue;
        }
        projects.extend(find_dirs_with_marker(root, marker, depth));
    }
    projects.sort();
    projects.dedup();

    let candidates: Vec<PathBuf> = projects
        .into_iter()
        .filter(|p| {
            let junk_path = p.join(junk);
            junk_path.is_dir() && older_than_days(&junk_path, days)
        })
        .collect();

    info!(found = candidates.len(), "candidates");

    if candidates.is_empty() {
        return Ok(());
    }

    let total_bytes = Arc::new(AtomicU64::new(0));
    let total_count = Arc::new(AtomicU64::new(0));
    let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
    let bar = Arc::new(CommandBar::new(bar_label, candidates.len() as u64));

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set: JoinSet<()> = JoinSet::new();
    for project in candidates {
        let sem = Arc::clone(&sem);
        let total_bytes = Arc::clone(&total_bytes);
        let total_count = Arc::clone(&total_count);
        let bar = Arc::clone(&bar);
        let items = Arc::clone(&items);
        set.spawn(
            async move {
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                clean_one(
                    project,
                    junk,
                    dry_run,
                    &total_bytes,
                    &total_count,
                    &bar,
                    &items,
                )
                .await;
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
    ui::print_tree(&format!("{bar_label}: {summary}"), &items);

    Ok(())
}

async fn clean_one(
    project: PathBuf,
    junk: &str,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
) {
    let junk_path = project.join(junk);
    let label = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.display().to_string());

    let started = Instant::now();
    let size = dir_size(&junk_path).await.unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let det = format!("would delete {}", format_size(size, BINARY));
        info!("✓ {label}  {}", det);
        (true, det)
    } else {
        match tokio::fs::remove_dir_all(&junk_path).await {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let det = format!(
                    "deleted {} in {}ms",
                    format_size(size, BINARY),
                    started.elapsed().as_millis()
                );
                info!("✓ {label}  {}", det);
                (true, det)
            }
            Err(e) => {
                let det = format!("failed: {e}");
                warn!("✗ {label}  {}", det);
                (false, det)
            }
        }
    };

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    bar.set_message(format!("{} freed", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}
