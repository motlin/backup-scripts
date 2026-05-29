use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::{CleanLibraryCachesConfig, expand_tildes};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 0;
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Compiled-in defaults. GUI-app and dev-tool caches under `~/Library/Caches` that
/// have no dedicated CLI (so the per-tool cleaners don't cover them) and that are
/// safe to scrub — i.e. the app regenerates them on next launch.
///
/// Notable exclusions:
/// - `iMazing`: holds real iOS device backups, not cache.
/// - `SiriTTS`, `GeoServices`, `com.apple.*`: macOS-managed system caches.
/// - `claude-cli-nodejs`: this process's own cache; touching it mid-run is a foot-gun.
const DEFAULT_PATHS: &[&str] = &[
    "~/Library/Caches/Google/Chrome",
    "~/Library/Caches/com.spotify.client",
    "~/Library/Caches/Steam",
    "~/Library/Caches/com.colliderli.iina",
    "~/Library/Caches/termius-updater",
    "~/Library/Caches/@spotlightjsspotlight-updater",
    "~/Library/Caches/superhuman-updater",
    "~/Library/Caches/virtualenv",
    "~/Library/Caches/bazelisk",
    "~/Library/Caches/typescript",
];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Cache dirs to scrub. May be repeated. [config: clean_library_caches.paths]
    #[arg(long)]
    pub path: Vec<PathBuf>,

    /// Only delete child entries older than this many days. 0 = always clean. [config: clean_library_caches.days, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions across cache dirs. [config: clean_library_caches.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(
    args: Args,
    cfg: &CleanLibraryCachesConfig,
    dry_run: bool,
) -> Result<CommandSummary> {
    let paths = if !args.path.is_empty() {
        expand_tildes(args.path)
    } else if let Some(p) = cfg.paths.as_ref().filter(|p| !p.is_empty()) {
        expand_tildes(p.clone())
    } else {
        default_paths()
    };
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    let result = async move {
        let candidates: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_dir()).collect();
        info!(found = candidates.len(), "existing cache dirs");

        if candidates.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new(
            "clean-library-caches",
            candidates.len() as u64,
        ));

        let max_label = candidates
            .iter()
            .map(|p| path_label(p).chars().count())
            .max()
            .unwrap_or(0);
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let label = path_label(&dir);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        dir,
                        max_label,
                        days,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                    )
                    .await;
                }
                .instrument(info_span!("cache", name = %label)),
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

        Ok::<_, anyhow::Error>(Some((summary, items, bytes)))
    }
    .instrument(info_span!("clean-library-caches", days))
    .await?;

    if let Some((summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::print_tree(&format!("clean-library-caches: {summary}"), &items);
        Ok(CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
            items_skipped: 0,
        })
    } else {
        Ok(CommandSummary::default())
    }
}

/// Use the last two components of the path (e.g. `Google/Chrome`,
/// `Caches/com.spotify.client`) so updater-suffixed entries stay distinguishable.
fn path_label(path: &Path) -> String {
    let mut comps: Vec<String> = path
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    comps.reverse();
    if comps.is_empty() {
        path.display().to_string()
    } else {
        comps.join("/")
    }
}

#[allow(clippy::too_many_arguments)]
async fn clean_one(
    dir: PathBuf,
    max_label: usize,
    days: u32,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
) {
    let label = path_label(&dir);
    let padded = pad_right(&label, max_label);

    let started = Instant::now();
    let children = match read_children(&dir) {
        Ok(c) => c,
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            warn!("✗ {padded}  {}", ui::format_detail(&detail));
            items.lock().unwrap().push(TreeItem {
                label,
                detail,
                ok: false,
            });
            bar.inc(1);
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
        if dry_run {
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

    total_bytes.fetch_add(freed, Ordering::Relaxed);
    total_count.fetch_add(1, Ordering::Relaxed);

    let detail = if !all_ok {
        let suffix = last_err.unwrap_or_else(|| "unknown error".to_string());
        ItemDetail::failure(suffix)
    } else if dry_run {
        ItemDetail::dry_run("would delete", format_size(freed, BINARY))
    } else {
        ItemDetail::success(
            "deleted",
            format_size(freed, BINARY),
            format_duration(started.elapsed().as_millis() as u64),
        )
    };

    let icon = if all_ok { "✓" } else { "✗" };
    info!("{icon} {padded}  {}", ui::format_detail(&detail));

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem {
        label,
        detail,
        ok: all_ok,
    });
}

fn read_children(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        out.push(entry.path());
    }
    Ok(out)
}

fn default_paths() -> Vec<PathBuf> {
    expand_tildes(DEFAULT_PATHS.iter().map(PathBuf::from).collect())
}
