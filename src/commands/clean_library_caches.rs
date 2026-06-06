use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use tracing::{Instrument, info, info_span};

use super::CommandSummary;
use super::executor::{delete_children, run_parallel};
use crate::config::{CleanLibraryCachesConfig, expand_tildes};

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

    async move {
        let candidates: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_dir()).collect();
        info!(found = candidates.len(), "existing cache dirs");

        Ok::<_, anyhow::Error>(
            run_parallel(
                "clean-library-caches",
                candidates,
                concurrency,
                dry_run,
                move |dir, progress| async move {
                    let label = path_label(&dir);
                    delete_children(&dir, days, label, &progress).await;
                },
            )
            .await,
        )
    }
    .instrument(info_span!("clean-library-caches", days))
    .await
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

fn default_paths() -> Vec<PathBuf> {
    expand_tildes(DEFAULT_PATHS.iter().map(PathBuf::from).collect())
}
