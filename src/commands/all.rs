use anyhow::{Result, bail};
use tracing::{Instrument, info_span, warn};

use crate::config::AppConfig;

use super::{
    bz_cleanup, clean_brew, clean_cargo, clean_cocoapods, clean_cypress, clean_docker,
    clean_go_build, clean_gradle, clean_jetbrains, clean_m2, clean_maven, clean_node,
    clean_node_gyp, clean_npm, clean_pip, clean_playwright, clean_pnpm, clean_tmp, clean_trash,
    clean_xcode, clean_yarn, git_maintenance,
};

pub const DEFAULT_STEPS: &[&str] = &[
    "git-maintenance",
    "clean-maven",
    "clean-node",
    "clean-cargo",
    "clean-m2",
    "clean-gradle",
    "clean-tmp",
    "clean-xcode",
    "clean-docker",
    "clean-brew",
    "clean-npm",
    "clean-yarn",
    "clean-pnpm",
    "clean-pip",
    "clean-cocoapods",
    "clean-go-build",
    "clean-jetbrains",
    "clean-playwright",
    "clean-cypress",
    "clean-node-gyp",
    "clean-trash",
    "bz-cleanup",
];

pub async fn run(config: &AppConfig, dry_run: bool) -> Result<()> {
    let steps: Vec<String> = config
        .all
        .steps
        .clone()
        .unwrap_or_else(|| DEFAULT_STEPS.iter().map(|s| s.to_string()).collect());

    async move {
        for step in &steps {
            match step.as_str() {
                "git-maintenance" => {
                    git_maintenance::run(
                        git_maintenance::Args::default(),
                        &config.git_maintenance,
                        &config.roots,
                        dry_run,
                    )
                    .await?
                }
                "clean-maven" => {
                    clean_maven::run(
                        clean_maven::Args::default(),
                        &config.clean_maven,
                        &config.roots,
                        dry_run,
                    )
                    .await?
                }
                "clean-node" => {
                    clean_node::run(
                        clean_node::Args::default(),
                        &config.clean_node,
                        &config.roots,
                        dry_run,
                    )
                    .await?
                }
                "clean-cargo" => {
                    clean_cargo::run(
                        clean_cargo::Args::default(),
                        &config.clean_cargo,
                        &config.roots,
                        dry_run,
                    )
                    .await?
                }
                "clean-m2" => {
                    clean_m2::run(clean_m2::Args::default(), &config.clean_m2, dry_run).await?
                }
                "clean-gradle" => {
                    clean_gradle::run(clean_gradle::Args::default(), &config.clean_gradle, dry_run)
                        .await?
                }
                "clean-tmp" => {
                    clean_tmp::run(clean_tmp::Args::default(), &config.clean_tmp, dry_run).await?
                }
                "clean-xcode" => {
                    clean_xcode::run(clean_xcode::Args::default(), &config.clean_xcode, dry_run)
                        .await?
                }
                "clean-docker" => {
                    clean_docker::run(clean_docker::Args::default(), &config.clean_docker, dry_run)
                        .await?
                }
                "clean-brew" => {
                    clean_brew::run(clean_brew::Args::default(), &config.clean_brew, dry_run)
                        .await?
                }
                "clean-npm" => {
                    clean_npm::run(clean_npm::Args::default(), &config.clean_npm, dry_run).await?
                }
                "clean-trash" => {
                    clean_trash::run(clean_trash::Args::default(), &config.clean_trash, dry_run)
                        .await?
                }
                "clean-yarn" => {
                    clean_yarn::run(clean_yarn::Args::default(), &config.clean_yarn, dry_run)
                        .await?
                }
                "clean-pnpm" => {
                    clean_pnpm::run(clean_pnpm::Args::default(), &config.clean_pnpm, dry_run)
                        .await?
                }
                "clean-pip" => {
                    clean_pip::run(clean_pip::Args::default(), &config.clean_pip, dry_run).await?
                }
                "clean-cocoapods" => {
                    clean_cocoapods::run(
                        clean_cocoapods::Args::default(),
                        &config.clean_cocoapods,
                        dry_run,
                    )
                    .await?
                }
                "clean-go-build" => {
                    clean_go_build::run(
                        clean_go_build::Args::default(),
                        &config.clean_go_build,
                        dry_run,
                    )
                    .await?
                }
                "clean-jetbrains" => {
                    clean_jetbrains::run(
                        clean_jetbrains::Args::default(),
                        &config.clean_jetbrains,
                        dry_run,
                    )
                    .await?
                }
                "clean-playwright" => {
                    clean_playwright::run(
                        clean_playwright::Args::default(),
                        &config.clean_playwright,
                        dry_run,
                    )
                    .await?
                }
                "clean-cypress" => {
                    clean_cypress::run(
                        clean_cypress::Args::default(),
                        &config.clean_cypress,
                        dry_run,
                    )
                    .await?
                }
                "clean-node-gyp" => {
                    clean_node_gyp::run(
                        clean_node_gyp::Args::default(),
                        &config.clean_node_gyp,
                        dry_run,
                    )
                    .await?
                }
                "bz-cleanup" => {
                    bz_cleanup::run(bz_cleanup::Args::default(), &config.bz_cleanup, dry_run)
                        .await?
                }
                unknown => {
                    bail!("unknown step in `all.steps`: {unknown}");
                }
            }
        }
        if steps.is_empty() {
            warn!("`all.steps` is empty — nothing to do");
        }
        Ok(())
    }
    .instrument(info_span!("all"))
    .await
}
