use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use super::executor::{delete_children, relative_label, run_parallel};
use crate::config::{CleanSteamConfig, expand_tilde};

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

    async move {
        if !steam_dir.is_dir() {
            warn!(path = %steam_dir.display(), "Steam dir not found, skipping");
            return Ok::<_, anyhow::Error>(CommandSummary::default());
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
            return Ok(CommandSummary::skipped_one());
        }

        let targets = collect_targets(&steam_dir, &cache_dirs);
        info!(found = targets.len(), "existing Steam cache dirs");

        let steam_for_labels = Arc::new(steam_dir.clone());
        Ok(run_parallel(
            "clean-steam",
            targets,
            concurrency,
            dry_run,
            move |dir, progress| {
                let steam_for_labels = Arc::clone(&steam_for_labels);
                async move {
                    let label = relative_label(&dir, &steam_for_labels);
                    delete_children(&dir, days, label, &progress).await;
                }
            },
        )
        .await)
    }
    .instrument(info_span!("clean-steam", days))
    .await
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
            relative_label(&steam.join("steamapps/shadercache"), steam),
            "steamapps/shadercache"
        );
        assert_eq!(relative_label(&steam.join("appcache"), steam), "appcache");
    }
}
