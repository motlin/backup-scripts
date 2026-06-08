use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use super::executor::{delete_dir, relative_label, run_parallel};
use crate::config::{CleanElectronCachesConfig, expand_tilde};
use crate::walk::older_than_days;

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
    /// Path to `~/Library/Application Support`. [config: `clean_electron_caches.support_dir`, default: ~/Library/Application Support]
    #[arg(long)]
    pub support_dir: Option<PathBuf>,

    /// App dir names to scrub. May be repeated. [config: `clean_electron_caches.apps`]
    #[arg(long = "app")]
    pub app: Vec<String>,

    /// Only delete cache subdirs older than this many days. 0 = always clean. [config: `clean_electron_caches.days`, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: `clean_electron_caches.concurrency`, default: 4]
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
        .map_or_else(default_support_dir, |p| expand_tilde(&p));
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

    async move {
        if !support_dir.exists() {
            info!(
                "Application Support dir does not exist, skipping: {}",
                support_dir.display()
            );
            return Ok(CommandSummary::default());
        }

        let candidates = find_cache_dirs(&support_dir, &apps);
        info!(found = candidates.len(), "candidate cache subdirs");

        let candidates: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|d| older_than_days(d, days))
            .collect();

        info!(after_mtime_filter = candidates.len(), "after --days filter");

        let support_for_labels = Arc::new(support_dir.clone());
        let summary = run_parallel(
            "clean-electron-caches",
            candidates,
            concurrency,
            dry_run,
            move |dir, progress| {
                let support_for_labels = Arc::clone(&support_for_labels);
                async move {
                    let label = relative_label(&dir, &support_for_labels);
                    delete_dir(&dir, label, &progress).await;
                }
            },
        )
        .await;

        Ok(summary)
    }
    .instrument(info_span!("clean-electron-caches", days))
    .await
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
    DEFAULT_APPS
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
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
        assert_eq!(relative_label(&dir, support), "Slack/Code Cache");
    }
}
