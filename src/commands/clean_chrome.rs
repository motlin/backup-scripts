use anyhow::{Result, bail};
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
use crate::config::{CleanChromeConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration, pad_right};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 0;
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const DEFAULT_CHROME_DIR: &str = "~/Library/Application Support/Google/Chrome";

/// Top-level model/asset bundles under the Chrome root. Chrome re-downloads
/// these on demand when the corresponding feature is re-enabled.
const DEFAULT_MODEL_DIRS: &[&str] = &[
    "OptGuideOnDeviceModel",
    "OptGuideOnDeviceClassifierModel",
    "optimization_guide_model_store",
    "SODA",
    "SODALanguagePacks",
    "WasmTtsEngine",
    "OnDeviceHeadSuggestModel",
];

/// Per-profile subdirs to scrub. `Service Worker` holds the bulk of cached
/// site state — sites re-register on next visit.
const DEFAULT_PROFILE_SUBDIRS: &[&str] = &["Service Worker"];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Chrome user-data root. [config: clean_chrome.chrome_dir, default: ~/Library/Application Support/Google/Chrome]
    #[arg(long)]
    pub chrome_dir: Option<PathBuf>,

    /// Only delete child entries older than this many days. 0 = always clean. [config: clean_chrome.days, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions across target dirs. [config: clean_chrome.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Skip the safety check that refuses to run while Chrome is open.
    /// Implied by --dry-run. [config: clean_chrome.skip_running_check, default: false]
    #[arg(long)]
    pub skip_running_check: bool,
}

pub async fn run(args: Args, cfg: &CleanChromeConfig, dry_run: bool) -> Result<CommandSummary> {
    let chrome_dir = args
        .chrome_dir
        .or_else(|| cfg.chrome_dir.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CHROME_DIR));
    let chrome_dir = expand_tilde(&chrome_dir);

    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let skip_running_check =
        args.skip_running_check || cfg.skip_running_check.unwrap_or(false) || dry_run;

    let model_dirs: Vec<String> = cfg
        .model_dirs
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL_DIRS.iter().map(|s| s.to_string()).collect());
    let profile_subdirs: Vec<String> = cfg.profile_subdirs.clone().unwrap_or_else(|| {
        DEFAULT_PROFILE_SUBDIRS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });

    let result = async move {
        if !chrome_dir.is_dir() {
            warn!(path = %chrome_dir.display(), "chrome dir not found");
            return Ok::<_, anyhow::Error>(None);
        }

        if !skip_running_check
            && let Some(pid) = chrome_running_pid().await
        {
            bail!(
                "Chrome appears to be running (PID {pid}); quit it before running clean-chrome (or pass --skip-running-check / --dry-run)"
            );
        }

        let targets = collect_targets(&chrome_dir, &model_dirs, &profile_subdirs);
        info!(found = targets.len(), "existing chrome target dirs");

        if targets.is_empty() {
            return Ok(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-chrome", targets.len() as u64));

        let max_label = targets
            .iter()
            .map(|p| path_label(p).chars().count())
            .max()
            .unwrap_or(0);
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in targets {
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
                .instrument(info_span!("target", name = %label)),
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

        Ok(Some((summary, items, bytes)))
    }
    .instrument(info_span!("clean-chrome", days))
    .await?;

    if let Some((summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::print_tree(&format!("clean-chrome: {summary}"), &items);
        Ok(CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
        })
    } else {
        Ok(CommandSummary::default())
    }
}

/// Returns the PID of any running Chrome process, or None if none are found.
/// Matches the main browser process name on macOS (`Google Chrome`); helper
/// and renderer processes share that prefix and are caught by the substring
/// match `pgrep -f` would do, but `-x` against the exact name is enough — if
/// the main process is alive the helpers are too.
async fn chrome_running_pid() -> Option<u32> {
    let output = tokio::process::Command::new("pgrep")
        .arg("-x")
        .arg("Google Chrome")
        .output()
        .await
        .ok()?;
    let stdout = std::str::from_utf8(&output.stdout).ok()?.trim();
    stdout.lines().next().and_then(|s| s.parse().ok())
}

fn collect_targets(
    chrome_dir: &Path,
    model_dirs: &[String],
    profile_subdirs: &[String],
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    for name in model_dirs {
        let p = chrome_dir.join(name);
        if p.is_dir() {
            out.push(p);
        }
    }

    let Ok(entries) = std::fs::read_dir(chrome_dir) else {
        return out;
    };
    let mut profiles: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "Default" || name.starts_with("Profile ") {
            profiles.push(path);
        }
    }
    profiles.sort();

    for profile in profiles {
        for sub in profile_subdirs {
            let p = profile.join(sub);
            if p.is_dir() {
                out.push(p);
            }
        }
    }

    out
}

/// Use the last two components of the path so `Default/Service Worker` and
/// `Profile 1/Service Worker` stay distinguishable in the tree output.
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
