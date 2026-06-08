use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use super::executor::{delete_children, run_parallel};
use crate::config::{CleanChromeConfig, expand_tilde};

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

/// Per-profile subdirs to scrub. `Service Worker/CacheStorage` holds offline
/// snapshots cached by service workers (the single largest Chrome cache on
/// disk) — sites re-fetch and re-cache on next visit. Scoped to `CacheStorage`
/// so the sibling `Service Worker/Database` (SW registrations) and
/// `Service Worker/ScriptCache` are left untouched. Auth lives in
/// `Cookies`/`IndexedDB` and is unaffected.
const DEFAULT_PROFILE_SUBDIRS: &[&str] = &["Service Worker/CacheStorage"];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Chrome user-data root. [config: `clean_chrome.chrome_dir`, default: ~/Library/Application Support/Google/Chrome]
    #[arg(long)]
    pub chrome_dir: Option<PathBuf>,

    /// Only delete child entries older than this many days. 0 = always clean. [config: `clean_chrome.days`, default: 0]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions across target dirs. [config: `clean_chrome.concurrency`, default: 4]
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Skip the safety check that refuses to run while Chrome is open.
    /// Implied by --dry-run. [config: `clean_chrome.skip_running_check`, default: false]
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

    let model_dirs: Vec<String> = cfg.model_dirs.clone().unwrap_or_else(|| {
        DEFAULT_MODEL_DIRS
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    });
    let profile_subdirs: Vec<String> = cfg.profile_subdirs.clone().unwrap_or_else(|| {
        DEFAULT_PROFILE_SUBDIRS
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    });

    async move {
        if !chrome_dir.is_dir() {
            warn!(path = %chrome_dir.display(), "chrome dir not found");
            return Ok::<_, anyhow::Error>(CommandSummary::default());
        }

        // Chrome holds locks on these caches while open; deleting under it can
        // corrupt the running browser. Unlike a hard error, we skip the step so an
        // open Chrome doesn't fail an otherwise-clean `all` run.
        if !skip_running_check && let Some(pid) = chrome_running_pid().await {
            warn!(pid, "Chrome appears to be running; skipping clean-chrome");
            info!(
                target: "tree",
                "skipped — quit Chrome and re-run, or pass --skip-running-check / --dry-run"
            );
            return Ok(CommandSummary::skipped_one());
        }

        let targets = collect_targets(&chrome_dir, &model_dirs, &profile_subdirs);
        info!(found = targets.len(), "existing chrome target dirs");

        Ok(run_parallel(
            "clean-chrome",
            targets,
            concurrency,
            dry_run,
            move |dir, progress| async move {
                let label = path_label(&dir);
                delete_children(&dir, days, label, &progress).await;
            },
        )
        .await)
    }
    .instrument(info_span!("clean-chrome", days))
    .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Unique, self-cleaning fixture root. `mkdtemp` guarantees a fresh directory
    /// per call (no pid collisions), and the `TempDir` removes it on drop even if
    /// the test panics.
    fn fixture_root(tag: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("backup-clean-chrome-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn default_subdirs() -> Vec<String> {
        DEFAULT_PROFILE_SUBDIRS
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    }

    #[test]
    fn default_profile_subdir_is_scoped_to_cachestorage() {
        assert_eq!(
            DEFAULT_PROFILE_SUBDIRS,
            &["Service Worker/CacheStorage"],
            "must scope to the CacheStorage subdir, not the whole Service Worker dir"
        );
    }

    #[test]
    fn collect_targets_selects_cachestorage_not_sw_siblings() {
        let fixture = fixture_root("cachestorage");
        let chrome = fixture.path();
        let sw = chrome.join("Default").join("Service Worker");
        // The cache we want to scrub.
        std::fs::create_dir_all(sw.join("CacheStorage")).unwrap();
        // Siblings that must NEVER be selected: SW registrations + script cache.
        std::fs::create_dir_all(sw.join("Database")).unwrap();
        std::fs::create_dir_all(sw.join("ScriptCache")).unwrap();
        // Auth state that must NEVER be selected.
        std::fs::create_dir_all(chrome.join("Default").join("IndexedDB")).unwrap();

        let targets = collect_targets(chrome, &[], &default_subdirs());

        assert_eq!(
            targets,
            vec![sw.join("CacheStorage")],
            "only Service Worker/CacheStorage, never Database/ScriptCache/IndexedDB"
        );
    }

    #[test]
    fn collect_targets_iterates_all_profiles() {
        let fixture = fixture_root("profiles");
        let chrome = fixture.path();
        for profile in ["Default", "Profile 1", "Profile 2"] {
            std::fs::create_dir_all(
                chrome
                    .join(profile)
                    .join("Service Worker")
                    .join("CacheStorage"),
            )
            .unwrap();
        }
        // A non-profile dir that happens to contain the subdir must be ignored.
        std::fs::create_dir_all(
            chrome
                .join("System Profile")
                .join("Service Worker")
                .join("CacheStorage"),
        )
        .unwrap();

        let targets = collect_targets(chrome, &[], &default_subdirs());

        assert_eq!(
            targets,
            vec![
                chrome
                    .join("Default")
                    .join("Service Worker")
                    .join("CacheStorage"),
                chrome
                    .join("Profile 1")
                    .join("Service Worker")
                    .join("CacheStorage"),
                chrome
                    .join("Profile 2")
                    .join("Service Worker")
                    .join("CacheStorage"),
            ],
            "every Default/Profile N dir is scanned; non-profile dirs are skipped"
        );
    }

    #[test]
    fn collect_targets_skips_missing_cachestorage() {
        let fixture = fixture_root("missing");
        let chrome = fixture.path();
        // Profile exists with a Service Worker dir but no CacheStorage child.
        std::fs::create_dir_all(
            chrome
                .join("Default")
                .join("Service Worker")
                .join("Database"),
        )
        .unwrap();

        let targets = collect_targets(chrome, &[], &default_subdirs());

        assert!(
            targets.is_empty(),
            "no CacheStorage dir means nothing to clean"
        );
    }
}
