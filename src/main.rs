mod commands;
mod config;
mod logging;
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
    All,

    /// Delete old Backblaze bz_done_*.dat files to reclaim disk space.
    BzCleanup(commands::bz_cleanup::Args),

    /// Run `git maintenance run` across every discovered git repo.
    GitMaintenance(commands::git_maintenance::Args),

    /// Delete stale Maven target/ directories.
    CleanMaven(commands::clean_maven::Args),

    /// Delete stale node_modules/ directories.
    CleanNode(commands::clean_node::Args),

    /// Delete stale Cargo target/ directories.
    CleanCargo(commands::clean_cargo::Args),

    /// Delete stale version directories from the local Maven repository (~/.m2/repository).
    CleanM2(commands::clean_m2::Args),

    /// Delete stale version directories from the Gradle modules cache (~/.gradle/caches/modules-2/files-2.1).
    CleanGradle(commands::clean_gradle::Args),

    /// Delete old files and directories from /tmp and other temp locations.
    CleanTmp(commands::clean_tmp::Args),

    /// Prune unused Docker images, containers, volumes, build cache, and networks.
    CleanDocker(commands::clean_docker::Args),
}

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
        Command::All => commands::all::run(&cfg, cli.dry_run).await,
        Command::BzCleanup(args) => {
            commands::bz_cleanup::run(args, &cfg.bz_cleanup, cli.dry_run).await
        }
        Command::GitMaintenance(args) => {
            commands::git_maintenance::run(args, &cfg.git_maintenance, &cfg.roots, cli.dry_run)
                .await
        }
        Command::CleanMaven(args) => {
            commands::clean_maven::run(args, &cfg.clean_maven, &cfg.roots, cli.dry_run).await
        }
        Command::CleanNode(args) => {
            commands::clean_node::run(args, &cfg.clean_node, &cfg.roots, cli.dry_run).await
        }
        Command::CleanCargo(args) => {
            commands::clean_cargo::run(args, &cfg.clean_cargo, &cfg.roots, cli.dry_run).await
        }
        Command::CleanM2(args) => commands::clean_m2::run(args, &cfg.clean_m2, cli.dry_run).await,
        Command::CleanGradle(args) => {
            commands::clean_gradle::run(args, &cfg.clean_gradle, cli.dry_run).await
        }
        Command::CleanTmp(args) => {
            commands::clean_tmp::run(args, &cfg.clean_tmp, cli.dry_run).await
        }
        Command::CleanDocker(args) => {
            commands::clean_docker::run(args, &cfg.clean_docker, cli.dry_run).await
        }
    }
}
