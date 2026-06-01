use anyhow::Result;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, warn};

use super::CommandSummary;
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
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
) -> Result<CommandSummary> {
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
        return Ok(CommandSummary::default());
    }

    let total_bytes = Arc::new(AtomicU64::new(0));
    let total_count = Arc::new(AtomicU64::new(0));
    let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
    let bar = Arc::new(CommandBar::new(bar_label, candidates.len() as u64));

    let labeled: Vec<(PathBuf, String)> = candidates
        .into_iter()
        .map(|p| {
            let label = project_label(&p, &roots);
            (p, label)
        })
        .collect();
    let max_label = labeled
        .iter()
        .map(|(_, l)| l.chars().count())
        .max()
        .unwrap_or(0);

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set: JoinSet<()> = JoinSet::new();
    for (project, label) in labeled {
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
                    label,
                    max_label,
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
    let items_ok = items.iter().filter(|i| i.ok).count() as u64;
    let items_failed = items.len() as u64 - items_ok;
    ui::print_tree(&format!("{bar_label}: {summary}"), &items);

    Ok(CommandSummary {
        bytes_freed: bytes,
        items_ok,
        items_failed,
        items_skipped: 0,
    })
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn project_label(project: &Path, roots: &[PathBuf]) -> String {
    // Longest matching root → shortest, most specific relative path.
    let best = roots
        .iter()
        .filter(|r| project.starts_with(r))
        .max_by_key(|r| r.components().count());
    if let Some(root) = best
        && let Ok(rel) = project.strip_prefix(root)
    {
        if !rel.as_os_str().is_empty() {
            return rel.display().to_string();
        }
        // project IS the root (marker at root top): use root basename.
        return basename(root);
    }
    basename(project) // defensive fallback
}

#[allow(clippy::too_many_arguments)]
async fn clean_one(
    project: PathBuf,
    label: String,
    max_label: usize,
    junk: &str,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
) {
    let junk_path = project.join(junk);
    let padded = pad_right(&label, max_label);

    let started = Instant::now();
    let size = dir_size(&junk_path).await.unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        info!("✓ {padded}  {}", ui::format_detail(&detail));
        (true, detail)
    } else {
        match tokio::fs::remove_dir_all(&junk_path).await {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let detail = ItemDetail::success(
                    "deleted",
                    format_size(size, BINARY),
                    format_duration(started.elapsed().as_millis() as u64),
                );
                info!("✓ {padded}  {}", ui::format_detail(&detail));
                (true, detail)
            }
            Err(e) => {
                let detail = ItemDetail::failure(format!("{e}"));
                warn!("✗ {padded}  {}", ui::format_detail(&detail));
                (false, detail)
            }
        }
    };

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

#[cfg(test)]
mod tests {
    use super::project_label;
    use std::path::{Path, PathBuf};

    #[test]
    fn nested_project_under_root_yields_relative_path() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects/motlin.com/cli");
        assert_eq!(project_label(project, &roots), "motlin.com/cli");
    }

    #[test]
    fn deeper_monorepo_path_yields_full_chain() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects/foo/packages/web");
        assert_eq!(project_label(project, &roots), "foo/packages/web");
    }

    #[test]
    fn project_equal_to_root_yields_root_basename() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects");
        assert_eq!(project_label(project, &roots), "projects");
    }

    #[test]
    fn longest_matching_root_wins() {
        let roots = vec![
            PathBuf::from("/home/user/projects"),
            PathBuf::from("/home/user/projects/motlin.com"),
        ];
        let project = Path::new("/home/user/projects/motlin.com/cli");
        assert_eq!(project_label(project, &roots), "cli");
    }

    #[test]
    fn no_matching_root_falls_back_to_basename() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/var/tmp/orphan/web");
        assert_eq!(project_label(project, &roots), "web");
    }
}
