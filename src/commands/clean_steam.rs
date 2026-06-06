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
use crate::config::{CleanSteamConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 0;
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const DEFAULT_STEAM_DIR: &str = "~/Library/Application Support/Steam";

/// Regenerable Steam caches, given relative to the Steam root. Steam rebuilds
/// each of these on demand:
/// - `steamapps/shadercache`: compiled GPU shader caches per installed game.
/// - `steamapps/downloading`: staging area for in-flight downloads/updates.
/// - `appcache`: client metadata caches (library art, app info).
///
/// We scrub each dir's CONTENTS but keep the dir itself. We never touch
/// `steamapps/common` (installed games) or the `steamapps/*.acf` manifests, so
/// only the three paths below are eligible.
const DEFAULT_CACHE_DIRS: &[&str] = &["steamapps/shadercache", "steamapps/downloading", "appcache"];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Steam root. [config: clean_steam.steam_dir, default: ~/Library/Application Support/Steam]
    #[arg(long)]
    pub steam_dir: Option<PathBuf>,

    /// Only delete child entries older than this many days. 0 = always clean. [config: clean_steam.days, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions across target dirs. [config: clean_steam.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Skip the safety check that refuses to run while Steam is open.
    /// Implied by --dry-run. [config: clean_steam.skip_running_check, default: false]
    #[arg(long)]
    pub skip_running_check: bool,
}

pub async fn run(args: Args, cfg: &CleanSteamConfig, dry_run: bool) -> Result<CommandSummary> {
    let steam_dir = args
        .steam_dir
        .or_else(|| cfg.steam_dir.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STEAM_DIR));
    let steam_dir = expand_tilde(&steam_dir);

    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let skip_running_check =
        args.skip_running_check || cfg.skip_running_check.unwrap_or(false) || dry_run;

    let cache_dirs: Vec<String> = cfg
        .cache_dirs
        .clone()
        .unwrap_or_else(|| DEFAULT_CACHE_DIRS.iter().map(|s| s.to_string()).collect());

    let result = async move {
        if !steam_dir.is_dir() {
            warn!(path = %steam_dir.display(), "Steam dir not found, skipping");
            return Ok::<_, anyhow::Error>(None);
        }

        // Steam holds file locks on these caches while open; deleting under it can
        // corrupt the running client. Unlike a hard error, we skip the step so an
        // open Steam doesn't fail an otherwise-clean `all` run.
        if !skip_running_check
            && let Some(pid) = steam_running_pid().await
        {
            warn!(
                pid,
                "Steam appears to be running; skipping clean-steam (quit Steam, or pass --skip-running-check / --dry-run)"
            );
            return Ok(None);
        }

        let targets = collect_targets(&steam_dir, &cache_dirs);
        info!(found = targets.len(), "existing Steam cache dirs");

        if targets.is_empty() {
            return Ok(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new("clean-steam", targets.len() as u64));

        let steam_for_labels = Arc::new(steam_dir.clone());
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in targets {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let steam_for_labels = Arc::clone(&steam_for_labels);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        dir,
                        days,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                        &steam_for_labels,
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

        Ok(Some((summary, items, bytes)))
    }
    .instrument(info_span!("clean-steam", days))
    .await?;

    if let Some((summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::print_tree(&format!("clean-steam: {summary}"), &items);
        Ok(CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
            items_skipped: 0,
        })
    } else {
        Ok(CommandSummary::skipped_one())
    }
}

/// Returns the PID of any running Steam process, or None if none are found.
/// The macOS app process is `steam_osx`; helpers share that name so matching the
/// main process is enough to know Steam is up.
async fn steam_running_pid() -> Option<u32> {
    let output = tokio::process::Command::new("pgrep")
        .arg("-x")
        .arg("steam_osx")
        .output()
        .await
        .ok()?;
    let stdout = std::str::from_utf8(&output.stdout).ok()?.trim();
    stdout.lines().next().and_then(|s| s.parse().ok())
}

/// Resolve each configured cache dir against the Steam root, keeping only those
/// that exist as directories.
fn collect_targets(steam_dir: &Path, cache_dirs: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for name in cache_dirs {
        let p = steam_dir.join(name);
        if p.is_dir() {
            out.push(p);
        }
    }
    out
}

/// Path relative to the Steam root (e.g. `steamapps/shadercache`), so the
/// two `steamapps/*` targets stay distinguishable from `appcache`.
fn target_label(path: &Path, steam_dir: &Path) -> String {
    path.strip_prefix(steam_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn clean_one(
    dir: PathBuf,
    days: u32,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    steam_dir: &Path,
) {
    let label = target_label(&dir, steam_dir);

    let started = Instant::now();
    let children = match read_children(&dir) {
        Ok(c) => c,
        Err(e) => {
            let detail = ItemDetail::failure(format!("{e}"));
            warn!("✗ {label}  {}", ui::format_detail(&detail));
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

    if !all_ok {
        warn!("✗ {label}  {}", ui::format_detail(&detail));
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, self-cleaning fixture root under the OS temp dir. Scoped by
    /// pid + `tag` so concurrent tests don't collide; removed and recreated fresh.
    fn fixture_root(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("backup-clean-steam-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn default_cache_dirs() -> Vec<String> {
        DEFAULT_CACHE_DIRS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_cache_dirs_are_the_three_regenerable_caches() {
        assert_eq!(
            DEFAULT_CACHE_DIRS,
            &["steamapps/shadercache", "steamapps/downloading", "appcache"],
            "only the regenerable caches; never steamapps/common or *.acf manifests"
        );
    }

    #[test]
    fn collect_targets_selects_only_existing_cache_dirs() {
        let steam = fixture_root("targets");
        std::fs::create_dir_all(steam.join("steamapps/shadercache")).unwrap();
        std::fs::create_dir_all(steam.join("appcache")).unwrap();
        // steamapps/downloading absent → not selected.

        let targets = collect_targets(&steam, &default_cache_dirs());

        assert_eq!(
            targets,
            vec![steam.join("steamapps/shadercache"), steam.join("appcache")],
            "preserves configured order, dropping the missing downloading dir"
        );

        std::fs::remove_dir_all(&steam).unwrap();
    }

    #[test]
    fn collect_targets_never_selects_installed_games_or_manifests() {
        let steam = fixture_root("games");
        // The valuable user data that must NEVER be a target.
        std::fs::create_dir_all(steam.join("steamapps/common/Half-Life")).unwrap();
        std::fs::write(steam.join("steamapps/appmanifest_70.acf"), b"x").unwrap();
        // A real cache that should be selected.
        std::fs::create_dir_all(steam.join("steamapps/shadercache")).unwrap();

        let targets = collect_targets(&steam, &default_cache_dirs());

        assert_eq!(
            targets,
            vec![steam.join("steamapps/shadercache")],
            "installed games and .acf manifests are never eligible"
        );

        std::fs::remove_dir_all(&steam).unwrap();
    }

    #[test]
    fn target_label_is_relative_to_steam_root() {
        let steam = Path::new("/Users/me/Library/Application Support/Steam");
        assert_eq!(
            target_label(&steam.join("steamapps/shadercache"), steam),
            "steamapps/shadercache"
        );
        assert_eq!(target_label(&steam.join("appcache"), steam), "appcache");
    }
}
