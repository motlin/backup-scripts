use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use humansize::{BINARY, format_size};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info};

use super::CommandSummary;
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration};
use crate::walk::{dir_size, older_than_days};

/// Shared progress state for one command's parallel clean: running totals,
/// the progress bar, and the per-item results tree.
pub struct CleanProgress {
    dry_run: bool,
    total_bytes: AtomicU64,
    total_count: AtomicU64,
    bar: CommandBar,
    items: Mutex<Vec<TreeItem>>,
}

impl CleanProgress {
    fn new(bar_label: &str, total: u64, dry_run: bool) -> Self {
        Self {
            dry_run,
            total_bytes: AtomicU64::new(0),
            total_count: AtomicU64::new(0),
            bar: CommandBar::new(bar_label, total),
            items: Mutex::new(Vec::new()),
        }
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    fn verb(&self) -> &'static str {
        if self.dry_run { "would free" } else { "freed" }
    }

    /// Record one finished item: accumulate freed bytes, count the item when it
    /// succeeded (or freed anything despite failing partway), advance the bar,
    /// and refresh the running total in the bar message.
    pub fn record(&self, label: String, detail: ItemDetail, ok: bool, bytes: u64) {
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        if ok || bytes > 0 {
            self.total_count.fetch_add(1, Ordering::Relaxed);
        }
        self.bar.inc(1);
        let running_bytes = self.total_bytes.load(Ordering::Relaxed);
        self.bar.set_message(format!(
            "{} {}",
            self.verb(),
            format_size(running_bytes, BINARY)
        ));
        self.items
            .lock()
            .unwrap()
            .push(TreeItem { label, detail, ok });
    }

    /// Consume the progress state: finalize the bar, emit the results tree and a
    /// closing summary event, and fold everything into a `CommandSummary`. The
    /// summary event is emitted inside the still-open command span so the
    /// `TreeLayer` renders it as the `└─` closing branch.
    fn finish(self) -> CommandSummary {
        let bytes = self.total_bytes.load(Ordering::Relaxed);
        let count = self.total_count.load(Ordering::Relaxed);
        let summary = format!(
            "{} {} across {count} items",
            self.verb(),
            format_size(bytes, BINARY)
        );
        self.bar.finish_ok(summary.clone());

        let items = self.items.into_inner().unwrap_or_default();
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::emit_tree_items(&items);
        info!(target: "tree", "{summary}");

        CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
            items_skipped: 0,
        }
    }
}

/// Run `clean_one` over every candidate with bounded concurrency, tracking
/// progress in a shared [`CleanProgress`]. Owns the whole lifecycle: empty
/// input short-circuits to a default summary, otherwise bar creation, the
/// semaphore-bounded spawn loop, and the final summary/tree printout.
pub async fn run_parallel<T, F, Fut>(
    bar_label: &'static str,
    candidates: Vec<T>,
    concurrency: usize,
    dry_run: bool,
    clean_one: F,
) -> CommandSummary
where
    T: Send + 'static,
    F: Fn(T, Arc<CleanProgress>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if candidates.is_empty() {
        let verb = if dry_run { "would free" } else { "freed" };
        info!(target: "tree", "{verb} 0 B across 0 items");
        return CommandSummary::default();
    }

    let progress = Arc::new(CleanProgress::new(
        bar_label,
        candidates.len() as u64,
        dry_run,
    ));
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set: JoinSet<()> = JoinSet::new();
    for candidate in candidates {
        let sem = Arc::clone(&sem);
        let progress = Arc::clone(&progress);
        let clean_one = clean_one.clone();
        set.spawn(
            async move {
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                clean_one(candidate, progress).await;
            }
            .in_current_span(),
        );
    }
    while set.join_next().await.is_some() {}

    let progress = Arc::try_unwrap(progress).unwrap_or_else(|_| panic!("progress arc leaked"));
    progress.finish()
}

/// Shared `clean_one` body for commands that delete a whole directory:
/// measure it, delete it (or pretend to in dry-run mode), record the outcome.
pub async fn delete_dir(dir: &Path, label: String, progress: &CleanProgress) {
    let started = Instant::now();
    let size = dir_size(dir).await.unwrap_or(0);

    if progress.dry_run() {
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        progress.record(label, detail, true, size);
        return;
    }

    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {
            let detail = ItemDetail::success(
                "deleted",
                format_size(size, BINARY),
                format_duration(started.elapsed().as_millis()),
            );
            progress.record(label, detail, true, size);
        }
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            progress.record(label, detail, false, 0);
        }
    }
}

/// Label a path by its location relative to `base`, falling back to the full
/// path when it is not under `base`. The standard `dir_label` for cleaners
/// whose candidates all live under one root.
pub fn relative_label(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Shared `clean_one` body for commands that delete a directory's CONTENTS but
/// keep the directory itself: list its children, delete each one older than
/// `days` (recursively for subdirs), accumulate freed bytes, and record a
/// single outcome for the parent. A failure to read the dir, or any child that
/// fails to delete, marks the item failed while still freeing what it can.
pub async fn delete_children(dir: &Path, days: u32, label: String, progress: &CleanProgress) {
    let started = Instant::now();
    let children = match read_children(dir) {
        Ok(c) => c,
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            progress.record(label, detail, false, 0);
            return;
        }
    };

    let mut freed: u64 = 0;
    let mut all_ok = true;
    let mut last_err: Option<String> = None;
    for child in &children {
        if !older_than_days(child, days) {
            continue;
        }
        let size = dir_size(child).await.unwrap_or(0);
        if progress.dry_run() {
            freed += size;
            continue;
        }
        let res = if child.is_dir() && !child.is_symlink() {
            tokio::fs::remove_dir_all(child).await
        } else {
            tokio::fs::remove_file(child).await
        };
        match res {
            Ok(()) => freed += size,
            Err(e) => {
                all_ok = false;
                last_err = Some(format!("{e}"));
            }
        }
    }

    let detail = if !all_ok {
        ItemDetail::failure(last_err.unwrap_or_else(|| "unknown error".to_string()))
    } else if progress.dry_run() {
        ItemDetail::dry_run("would delete", format_size(freed, BINARY))
    } else {
        ItemDetail::success(
            "deleted",
            format_size(freed, BINARY),
            format_duration(started.elapsed().as_millis()),
        )
    };

    progress.record(label, detail, all_ok, freed);
}

fn read_children(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        out.push(entry?.path());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use crate::logging::TreeLayer;
    use crate::ui::ItemDetail;

    use super::{CleanProgress, run_parallel};

    fn progress() -> CleanProgress {
        CleanProgress::new("test", 3, false)
    }

    /// Drive a `run_parallel` future to completion inside a command span under a
    /// capturing `TreeLayer`, returning the rendered tree lines. A current-thread
    /// runtime lets us block on the future while the span is entered, so the
    /// summary event in `finish()` lands inside the still-open span.
    fn capture_run_parallel<Fut>(body: impl FnOnce() -> Fut) -> Vec<String>
    where
        Fut: std::future::Future,
    {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let layer = TreeLayer::new(
            move |line: &str| sink.lock().unwrap().push(line.to_string()),
            false,
        );
        let subscriber = Registry::default().with(layer);
        with_default(subscriber, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("current-thread runtime builds");
            let _ = rt.block_on(body());
        });
        Arc::into_inner(lines)
            .expect("layer dropped its Arc clone after with_default returned")
            .into_inner()
            .expect("capture mutex is never poisoned")
    }

    #[test]
    fn record_success_counts_item_and_bytes() {
        let p = progress();
        p.record("a".to_string(), ItemDetail::failure("x"), true, 100);
        assert_eq!(p.total_bytes.load(Ordering::Relaxed), 100);
        assert_eq!(p.total_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_failure_with_zero_bytes_is_not_counted() {
        let p = progress();
        p.record("a".to_string(), ItemDetail::failure("x"), false, 0);
        assert_eq!(p.total_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(p.total_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_partial_failure_that_freed_bytes_is_counted() {
        let p = progress();
        p.record("a".to_string(), ItemDetail::failure("x"), false, 50);
        assert_eq!(p.total_bytes.load(Ordering::Relaxed), 50);
        assert_eq!(p.total_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_accumulates_bytes_across_items() {
        let p = progress();
        p.record("a".to_string(), ItemDetail::failure("x"), true, 100);
        p.record("b".to_string(), ItemDetail::failure("x"), true, 200);
        assert_eq!(p.total_bytes.load(Ordering::Relaxed), 300);
        assert_eq!(p.total_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn finish_derives_summary_from_recorded_items() {
        let p = progress();
        p.record("a".to_string(), ItemDetail::failure("x"), true, 100);
        p.record("b".to_string(), ItemDetail::failure("x"), true, 200);
        p.record("c".to_string(), ItemDetail::failure("x"), false, 0);
        let summary = p.finish();
        assert_eq!(summary.bytes_freed, 300);
        assert_eq!(summary.items_ok, 2);
        assert_eq!(summary.items_failed, 1);
        assert_eq!(summary.items_skipped, 0);
    }

    #[tokio::test]
    async fn run_parallel_empty_candidates_yields_default_summary() {
        let summary = run_parallel("test", Vec::<u64>::new(), 4, false, |_, _| async {}).await;
        assert_eq!(summary.bytes_freed, 0);
        assert_eq!(summary.items_ok, 0);
        assert_eq!(summary.items_failed, 0);
        assert_eq!(summary.items_skipped, 0);
    }

    #[tokio::test]
    async fn run_parallel_aggregates_recorded_outcomes() {
        let candidates: Vec<u64> = vec![10, 20, 0];
        let summary = run_parallel(
            "test",
            candidates,
            2,
            false,
            |bytes, progress: Arc<CleanProgress>| async move {
                let ok = bytes > 0;
                progress.record(format!("item-{bytes}"), ItemDetail::failure("x"), ok, bytes);
            },
        )
        .await;
        assert_eq!(summary.bytes_freed, 30);
        assert_eq!(summary.items_ok, 2);
        assert_eq!(summary.items_failed, 1);
        assert_eq!(summary.items_skipped, 0);
    }

    #[test]
    fn run_parallel_emits_item_branches_and_summary() {
        let out = capture_run_parallel(|| {
            run_parallel(
                "test",
                vec![10_u64, 20],
                1,
                false,
                |bytes, progress: Arc<CleanProgress>| async move {
                    progress.record(
                        format!("item-{bytes}"),
                        ItemDetail::failure("x"),
                        true,
                        bytes,
                    );
                },
            )
        });
        assert_eq!(out.first().map(String::as_str), Some(""));
        assert_eq!(out.get(1).map(String::as_str), Some("clean-x"));
        let branches: Vec<&String> = out.iter().filter(|l| l.starts_with("├─ ✓ ")).collect();
        assert_eq!(branches.len(), 2);
        assert_eq!(
            out.last().map(String::as_str),
            Some("└─ freed 30 B across 2 items")
        );
    }

    #[test]
    fn run_parallel_empty_candidates_emits_closing_summary() {
        let out = capture_run_parallel(|| {
            run_parallel("test", Vec::<u64>::new(), 1, false, |_, _| async {})
        });
        assert_eq!(
            out.last().map(String::as_str),
            Some("└─ freed 0 B across 0 items")
        );
    }

    #[test]
    fn run_parallel_empty_candidates_dry_run_uses_would_free_verb() {
        let out = capture_run_parallel(|| {
            run_parallel("test", Vec::<u64>::new(), 1, true, |_, _| async {})
        });
        assert_eq!(
            out.last().map(String::as_str),
            Some("└─ would free 0 B across 0 items")
        );
    }
}
