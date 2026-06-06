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
use crate::config::{CleanElectronCachesConfig, expand_tilde};
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration};
use crate::walk::{dir_size, older_than_days};

pub const DEFAULT_DAYS: u32 = 0;
pub const DEFAULT_CONCURRENCY: usize = 4;

/// The only subfolders we delete inside each Electron app's support dir. These
/// are pure HTTP/render caches that the app regenerates on next launch.
///
/// We deliberately NEVER touch sibling dirs such as `Local Storage`,
/// `IndexedDB`, `Cookies`, `Preferences`, or `Service Worker` — those hold
/// session tokens, mail indexes (Superhuman), and transcripts (Wispr Flow).
const CACHE_SUBDIRS: &[&str] = &["Cache", "Code Cache", "GPUCache"];

/// Compiled-in allowlist of Electron desktop apps under
/// `~/Library/Application Support/<App>`. Only these app dirs are inspected,
/// and only their `CACHE_SUBDIRS` are removed.
///
/// Notable exclusion: `Spotify` — it has no `Code Cache`/`GPUCache`, and its
/// `PersistentCache` holds paid offline downloads (a different, higher-value
/// tier). Cleaning that belongs elsewhere, not here.
const DEFAULT_APPS: &[&str] = &[
    "Slack",
    "discord",
    "Superhuman",
    "Termius",
    "WorkFlowy",
    "Wispr Flow",
    "Claude",
    "BraveSoftware",
];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Path to `~/Library/Application Support`. [config: clean_electron_caches.support_dir, default: ~/Library/Application Support]
    #[arg(long)]
    pub support_dir: Option<PathBuf>,

    /// App dir names to scrub. May be repeated. [config: clean_electron_caches.apps]
    #[arg(long = "app")]
    pub app: Vec<String>,

    /// Only delete cache subdirs older than this many days. 0 = always clean. [config: clean_electron_caches.days, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_electron_caches.concurrency, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(
    args: Args,
    cfg: &CleanElectronCachesConfig,
    dry_run: bool,
) -> Result<CommandSummary> {
    let support_dir = args
        .support_dir
        .or_else(|| cfg.support_dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(default_support_dir);
    let apps: Vec<String> = if !args.app.is_empty() {
        args.app
    } else if let Some(a) = cfg.apps.as_ref().filter(|a| !a.is_empty()) {
        a.clone()
    } else {
        default_apps()
    };
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);

    warn!("quit each Electron app before cleaning — an open app can corrupt its caches mid-delete");

    let result = async move {
        if !support_dir.exists() {
            info!(
                "Application Support dir does not exist, skipping: {}",
                support_dir.display()
            );
            return Ok::<_, anyhow::Error>(None);
        }

        let candidates = find_cache_dirs(&support_dir, &apps);
        info!(found = candidates.len(), "candidate cache subdirs");

        let candidates: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        if candidates.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let total_bytes = Arc::new(AtomicU64::new(0));
        let total_count = Arc::new(AtomicU64::new(0));
        let items: Arc<Mutex<Vec<TreeItem>>> = Arc::new(Mutex::new(Vec::new()));
        let bar = Arc::new(CommandBar::new(
            "clean-electron-caches",
            candidates.len() as u64,
        ));

        let support_for_labels = Arc::new(support_dir.clone());
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let mut set: JoinSet<()> = JoinSet::new();
        for dir in candidates {
            let sem = Arc::clone(&sem);
            let total_bytes = Arc::clone(&total_bytes);
            let total_count = Arc::clone(&total_count);
            let bar = Arc::clone(&bar);
            let items = Arc::clone(&items);
            let support_for_labels = Arc::clone(&support_for_labels);
            set.spawn(
                async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    clean_one(
                        dir,
                        dry_run,
                        &total_bytes,
                        &total_count,
                        &bar,
                        &items,
                        &support_for_labels,
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

        Ok::<_, anyhow::Error>(Some((summary, items, bytes)))
    }
    .instrument(info_span!("clean-electron-caches", days))
    .await?;

    if let Some((summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::print_tree(&format!("clean-electron-caches: {summary}"), &items);
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

fn dir_label(dir: &Path, support_dir: &Path) -> String {
    dir.strip_prefix(support_dir)
        .unwrap_or(dir)
        .display()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn clean_one(
    dir: PathBuf,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
    support_dir: &Path,
) {
    let label = dir_label(&dir, support_dir);

    let started = Instant::now();
    let size = dir_size(&dir).await.unwrap_or(0);

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        (true, detail)
    } else {
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let detail = ItemDetail::success(
                    "deleted",
                    format_size(size, BINARY),
                    format_duration(started.elapsed().as_millis() as u64),
                );
                (true, detail)
            }
            Err(e) => {
                let detail = ItemDetail::failure(format!("{e}"));
                warn!("✗ {label}  {}", ui::format_detail(&detail));
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

/// For each allowlisted app dir that exists under `support_dir`, collect the
/// `CACHE_SUBDIRS` that are present. Only existing directories are returned, so
/// missing apps and missing cache subdirs are silently skipped.
fn find_cache_dirs(support_dir: &Path, apps: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for app in apps {
        let app_dir = support_dir.join(app);
        for sub in cache_subdirs_for(&app_dir) {
            dirs.push(sub);
        }
    }
    dirs.sort();
    dirs
}

/// The `CACHE_SUBDIRS` that exist as directories directly under `app_dir`.
fn cache_subdirs_for(app_dir: &Path) -> Vec<PathBuf> {
    CACHE_SUBDIRS
        .iter()
        .map(|name| app_dir.join(name))
        .filter(|p| p.is_dir() && !p.is_symlink())
        .collect()
}

fn default_support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Application Support")
}

fn default_apps() -> Vec<String> {
    DEFAULT_APPS.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, self-cleaning fixture root under the OS temp dir. Scoped by
    /// pid + `tag` so concurrent tests don't collide; removed and recreated fresh.
    fn fixture_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "backup-clean-electron-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn cache_subdirs_for_returns_only_existing_cache_dirs() {
        let support = fixture_root("subdirs");
        let app = support.join("Slack");
        std::fs::create_dir_all(app.join("Cache")).unwrap();
        std::fs::create_dir_all(app.join("GPUCache")).unwrap();
        // "Code Cache" intentionally absent.
        // Sibling that must never be selected:
        std::fs::create_dir_all(app.join("Local Storage")).unwrap();

        let found = cache_subdirs_for(&app);

        assert_eq!(
            found,
            vec![app.join("Cache"), app.join("GPUCache")],
            "only existing CACHE_SUBDIRS, never siblings like Local Storage"
        );

        std::fs::remove_dir_all(&support).unwrap();
    }

    #[test]
    fn cache_subdirs_for_missing_app_is_empty() {
        let support = fixture_root("missing-app");
        let app = support.join("NotInstalled");
        assert!(cache_subdirs_for(&app).is_empty());
        std::fs::remove_dir_all(&support).unwrap();
    }

    #[test]
    fn find_cache_dirs_only_inspects_allowlisted_apps() {
        let support = fixture_root("allowlist");
        // Allowlisted app with caches.
        std::fs::create_dir_all(support.join("Slack").join("Cache")).unwrap();
        std::fs::create_dir_all(support.join("Slack").join("Code Cache")).unwrap();
        // Non-allowlisted app with a Cache dir — must be ignored entirely.
        std::fs::create_dir_all(support.join("Spotify").join("Cache")).unwrap();
        std::fs::create_dir_all(support.join("Evil").join("Cache")).unwrap();

        let apps = vec!["Slack".to_string()];
        let found = find_cache_dirs(&support, &apps);

        assert_eq!(
            found,
            vec![
                support.join("Slack").join("Cache"),
                support.join("Slack").join("Code Cache"),
            ],
            "Spotify and other non-allowlisted apps are never touched"
        );

        std::fs::remove_dir_all(&support).unwrap();
    }

    #[test]
    fn find_cache_dirs_never_selects_sensitive_siblings() {
        let support = fixture_root("siblings");
        let app = support.join("Superhuman");
        for sensitive in [
            "Local Storage",
            "IndexedDB",
            "Cookies",
            "Preferences",
            "Service Worker",
        ] {
            std::fs::create_dir_all(app.join(sensitive)).unwrap();
        }
        std::fs::create_dir_all(app.join("Cache")).unwrap();

        let apps = vec!["Superhuman".to_string()];
        let found = find_cache_dirs(&support, &apps);

        assert_eq!(found, vec![app.join("Cache")]);

        std::fs::remove_dir_all(&support).unwrap();
    }

    #[test]
    fn default_apps_excludes_spotify() {
        let apps = default_apps();
        assert!(
            !apps.iter().any(|a| a.eq_ignore_ascii_case("Spotify")),
            "Spotify holds paid offline downloads; it must not be in the allowlist"
        );
        assert!(apps.contains(&"Slack".to_string()));
        assert!(apps.contains(&"Wispr Flow".to_string()));
    }

    #[test]
    fn dir_label_is_relative_to_support_dir() {
        let support = Path::new("/Users/me/Library/Application Support");
        let dir = support.join("Slack").join("Code Cache");
        assert_eq!(dir_label(&dir, support), "Slack/Code Cache");
    }
}
