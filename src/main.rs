mod commands;
mod config;
mod logging;
#[cfg(test)]
mod test_support;
mod ui;
mod walk;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "backup", version, about = "Daily backup and maintenance tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Preview actions without making changes.
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run all configured maintenance steps in `all.steps` order.
    All(commands::all::Args),

    /// Delete old Backblaze `bz_done`_*.dat files to reclaim disk space.
    BzCleanup(commands::bz_cleanup::Args),

    /// Run aggressive maintenance (pack-refs, reflog expire, rerere gc, worktree prune,
    /// `repack -Adf --depth=100 --window=250`, prune --expire=1.week.ago, commit-graph write)
    /// on every discovered git repo. Includes objects from alternates.
    GitMaintenance(commands::git_maintenance::Args),

    /// Delete stale Maven target/ directories.
    CleanMaven(commands::clean_maven::Args),

    /// Delete stale `node_modules`/ directories.
    CleanNode(commands::clean_node::Args),

    /// Delete stale Cargo target/ directories.
    CleanCargo(commands::clean_cargo::Args),

    /// Delete stale version directories from the local Maven repository (~/.m2/repository).
    CleanM2(commands::clean_m2::Args),

    /// Delete stale version directories from the Gradle modules cache (~/.gradle/caches/modules-2/files-2.1).
    CleanGradle(commands::clean_gradle::Args),

    /// Delete old files and directories from /tmp and other temp locations.
    CleanTmp(commands::clean_tmp::Args),

    /// Delete stale per-project Xcode `DerivedData` directories.
    CleanXcode(commands::clean_xcode::Args),

    /// Prune unused Docker images, containers, build cache, and networks. Volume cleanup
    /// is opt-in. Measures and, when necessary, triggers Docker.raw physical reclamation.
    CleanDocker(commands::clean_docker::Args),

    /// Run `brew cleanup` to remove old formula/cask versions and prune the download cache.
    CleanBrew(commands::clean_brew::Args),

    /// Delete stale entries from the npm cacache directory (~/.npm/_cacache).
    CleanNpm(commands::clean_npm::Args),

    /// Empty the macOS Trash (~/.Trash). Deletes the contents of the Trash but
    /// preserves the directory itself.
    CleanTrash(commands::clean_trash::Args),

    /// Run `yarn cache clean` to clear the Yarn package cache.
    CleanYarn(commands::clean_yarn::Args),

    /// Run `pnpm store prune` to remove unreferenced packages from the pnpm store.
    CleanPnpm(commands::clean_pnpm::Args),

    /// Run `pip cache purge` to clear the pip download cache.
    CleanPip(commands::clean_pip::Args),

    /// Run `pod cache clean --all` to clear the `CocoaPods` download cache.
    CleanCocoapods(commands::clean_cocoapods::Args),

    /// Run `go clean -cache` to clear the Go build cache.
    CleanGoBuild(commands::clean_go_build::Args),

    /// Delete stale per-product `JetBrains` IDE caches (~/Library/Caches/JetBrains/*).
    /// Skips the `Toolbox` subdir (installed IDE binaries, not a cache). Close
    /// the relevant IDEs before running to avoid index corruption.
    CleanJetbrains(commands::clean_jetbrains::Args),

    /// Delete old log files from `~/Library/Logs`, keeping the folder tree so apps
    /// keep writing. Only files older than `--days` are removed. Skips
    /// `DiagnosticReports/` (`.ips` crash reports macOS auto-purges) and tolerates
    /// files locked by an app that is actively writing.
    CleanLogs(commands::clean_logs::Args),

    /// Scrub a configurable list of `~/Library/Caches` subdirs (Chrome, Spotify,
    /// Steam, IINA, app updaters, virtualenv, bazelisk, typescript, …). Removes
    /// each path's contents but keeps the parent dir intact.
    CleanLibraryCaches(commands::clean_library_caches::Args),

    /// Delete render/HTTP caches (Cache, Code Cache, `GPUCache`) from an allowlist
    /// of Electron desktop apps under `~/Library/Application Support/<App>`
    /// (Slack, Discord, Superhuman, Termius, `WorkFlowy`, Wispr Flow, Claude,
    /// Brave). Never touches Local Storage/IndexedDB/Cookies/Service Worker.
    /// Quit each app first — an open app can corrupt its caches mid-delete.
    CleanElectronCaches(commands::clean_electron_caches::Args),

    /// Reclaim space under `~/Library/Application Support/Google/Chrome` by
    /// scrubbing on-device model bundles (`OptGuide`*, SODA*, `WasmTtsEngine`, …)
    /// and per-profile `Service Worker/CacheStorage` offline snapshots (the
    /// largest Chrome cache; SW registrations and auth are left intact).
    /// Refuses to run while Chrome is open. Chrome re-downloads models on
    /// demand; sites re-cache on next visit.
    CleanChrome(commands::clean_chrome::Args),

    /// Scrub regenerable Steam caches under `~/Library/Application Support/Steam`:
    /// the CONTENTS of `steamapps/shadercache`, `steamapps/downloading`, and
    /// `appcache` (each dir is kept). Never touches `steamapps/common` (installed
    /// games) or the `steamapps/*.acf` manifests. Quit Steam first — an open
    /// client holds file locks and can corrupt these caches mid-delete; the step
    /// is skipped while Steam is running unless `--skip-running-check`/`--dry-run`.
    CleanSteam(commands::clean_steam::Args),

    /// Delete stale Playwright browser-version dirs (~/Library/Caches/ms-playwright/*).
    CleanPlaywright(commands::clean_playwright::Args),

    /// Delete stale Cypress binary version dirs (~/Library/Caches/Cypress/*).
    CleanCypress(commands::clean_cypress::Args),

    /// Delete stale node-gyp per-Node-version header dirs (~/Library/Caches/node-gyp/*).
    CleanNodeGyp(commands::clean_node_gyp::Args),

    /// Reclaim mise disk usage without removing in-use tools: `mise cache prune`
    /// (age-based cache GC) plus `mise prune` (removes tool versions no longer
    /// referenced by any tracked config). Versions pinned by a config are kept;
    /// `~/.local/share/mise/installs` is never touched directly.
    CleanMise(commands::clean_mise::Args),

    /// Reclaim disk under the XDG cache (`~/.cache`) via first-party subcommands:
    /// `uv cache prune` (unreachable entries; never a raw `rm`) and `pre-commit gc`
    /// (unused hook envs), plus a direct scrub of `~/.cache/node` contents (no
    /// managing CLI), age-gated by `--node-days`. Each op is skipped when its
    /// tool/dir is absent. `uv`/`pre-commit` have no dry-run flag, so `--dry-run`
    /// only logs their intent. Leaves `github-copilot`/`copilot` alone (re-auth risk).
    CleanXdgCache(commands::clean_xdg_cache::Args),

    /// Uninstall non-default rustup toolchains. Only removes toolchains that are
    /// neither active/default nor referenced by a directory override; always via
    /// `rustup toolchain uninstall` (never a raw `rm`). Preview-only unless
    /// `--remove` (or `clean_rustup.remove = true`) is set.
    CleanRustup(commands::clean_rustup::Args),

    /// Hard-link duplicate media across organized `iMessage` and complete
    /// `yt-dlp` directory trees. This command is not part of `backup all`.
    DedupeMedia(commands::dedupe_media::Args),
}

// A flat `match` over every subcommand; each arm is a couple of lines. The length
// is inherent to the dispatch table and would only be obscured by indirection.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load()?;

    walk::init_walk_options(walk::WalkOptions {
        prune_dirs: cfg
            .walk
            .prune_dirs
            .clone()
            .unwrap_or_else(|| walk::WalkOptions::fallback().prune_dirs),
        follow_symlinks: cfg.walk.follow_symlinks.unwrap_or(false),
    });

    ui::init();
    logging::init(&cfg.logging);

    match cli.command {
        Command::All(args) => commands::all::run(args, &cfg, cli.dry_run).await,
        Command::BzCleanup(args) => commands::bz_cleanup::run(args, &cfg.bz_cleanup, cli.dry_run)
            .await
            .map(drop),
        Command::GitMaintenance(args) => commands::git_maintenance::run(
            args,
            &cfg.git_maintenance,
            cfg.roots.as_ref(),
            cli.dry_run,
        )
        .await
        .map(drop),
        Command::CleanMaven(args) => {
            commands::clean_maven::run(args, &cfg.clean_maven, cfg.roots.as_ref(), cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanNode(args) => {
            commands::clean_node::run(args, &cfg.clean_node, cfg.roots.as_ref(), cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanCargo(args) => {
            commands::clean_cargo::run(args, &cfg.clean_cargo, cfg.roots.as_ref(), cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanM2(args) => commands::clean_m2::run(args, &cfg.clean_m2, cli.dry_run)
            .await
            .map(drop),
        Command::CleanGradle(args) => {
            commands::clean_gradle::run(args, &cfg.clean_gradle, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanTmp(args) => commands::clean_tmp::run(args, &cfg.clean_tmp, cli.dry_run)
            .await
            .map(drop),
        Command::CleanXcode(args) => {
            commands::clean_xcode::run(args, &cfg.clean_xcode, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanDocker(args) => {
            commands::clean_docker::run(args, &cfg.clean_docker, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanBrew(args) => commands::clean_brew::run(args, &cfg.clean_brew, cli.dry_run)
            .await
            .map(drop),
        Command::CleanNpm(args) => commands::clean_npm::run(args, &cfg.clean_npm, cli.dry_run)
            .await
            .map(drop),
        Command::CleanTrash(args) => {
            commands::clean_trash::run(args, &cfg.clean_trash, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanYarn(args) => commands::clean_yarn::run(args, &cfg.clean_yarn, cli.dry_run)
            .await
            .map(drop),
        Command::CleanPnpm(args) => commands::clean_pnpm::run(args, &cfg.clean_pnpm, cli.dry_run)
            .await
            .map(drop),
        Command::CleanPip(args) => commands::clean_pip::run(args, &cfg.clean_pip, cli.dry_run)
            .await
            .map(drop),
        Command::CleanCocoapods(args) => {
            commands::clean_cocoapods::run(args, &cfg.clean_cocoapods, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanGoBuild(args) => {
            commands::clean_go_build::run(args, &cfg.clean_go_build, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanJetbrains(args) => {
            commands::clean_jetbrains::run(args, &cfg.clean_jetbrains, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanLogs(args) => commands::clean_logs::run(args, &cfg.clean_logs, cli.dry_run)
            .await
            .map(drop),
        Command::CleanLibraryCaches(args) => {
            commands::clean_library_caches::run(args, &cfg.clean_library_caches, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanElectronCaches(args) => {
            commands::clean_electron_caches::run(args, &cfg.clean_electron_caches, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanChrome(args) => {
            commands::clean_chrome::run(args, &cfg.clean_chrome, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanSteam(args) => {
            commands::clean_steam::run(args, &cfg.clean_steam, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanPlaywright(args) => {
            commands::clean_playwright::run(args, &cfg.clean_playwright, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanCypress(args) => {
            commands::clean_cypress::run(args, &cfg.clean_cypress, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanNodeGyp(args) => {
            commands::clean_node_gyp::run(args, &cfg.clean_node_gyp, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanMise(args) => commands::clean_mise::run(args, &cfg.clean_mise, cli.dry_run)
            .await
            .map(drop),
        Command::CleanXdgCache(args) => {
            commands::clean_xdg_cache::run(args, &cfg.clean_xdg_cache, cli.dry_run)
                .await
                .map(drop)
        }
        Command::CleanRustup(args) => {
            commands::clean_rustup::run(args, &cfg.clean_rustup, cli.dry_run)
                .await
                .map(drop)
        }
        Command::DedupeMedia(args) => {
            commands::dedupe_media::run(args, &cfg.dedupe_media, cli.dry_run)
                .await
                .map(drop)
        }
    }
}
