use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::collections::HashSet;
use std::time::Instant;
use tracing::{Instrument, info, info_span, warn};

use crate::config::AppConfig;
use crate::ui::format_duration;

use super::{
    CommandSummary, bz_cleanup, clean_brew, clean_cargo, clean_chrome, clean_cocoapods,
    clean_cypress, clean_docker, clean_electron_caches, clean_go_build, clean_gradle,
    clean_jetbrains, clean_library_caches, clean_logs, clean_m2, clean_maven, clean_mise,
    clean_node, clean_node_gyp, clean_npm, clean_pip, clean_playwright, clean_pnpm, clean_rustup,
    clean_steam, clean_tmp, clean_trash, clean_xcode, clean_xdg_cache, clean_yarn, git_maintenance,
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
    "clean-logs",
    "clean-library-caches",
    "clean-electron-caches",
    "clean-chrome",
    "clean-steam",
    "clean-playwright",
    "clean-cypress",
    "clean-node-gyp",
    "clean-mise",
    "clean-xdg-cache",
    "clean-rustup",
    "clean-trash",
    "bz-cleanup",
];

struct StepResult {
    name: String,
    summary: CommandSummary,
}

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Skip a step (by name) from the resolved step list. Repeatable.
    /// Names match `all.steps` entries, e.g. `git-maintenance`, `clean-tmp`.
    #[arg(long = "skip")]
    pub skip: Vec<String>,

    /// Run only these steps (by name), in `all.steps` order. Inverse of `--skip`.
    /// Names match `all.steps` entries. Repeatable.
    #[arg(long = "only")]
    pub only: Vec<String>,
}

/// Validate that every name in `names` is a known step, returning an error
/// naming the first unknown one.
fn validate_steps(names: &[String]) -> Result<()> {
    let known: HashSet<&str> = DEFAULT_STEPS.iter().copied().collect();
    for name in names {
        if !known.contains(name.as_str()) {
            bail!("unknown step: {name}");
        }
    }
    Ok(())
}

pub async fn run(args: Args, config: &AppConfig, dry_run: bool) -> Result<()> {
    let mut steps: Vec<String> = config
        .all
        .steps
        .clone()
        .unwrap_or_else(|| DEFAULT_STEPS.iter().map(|s| s.to_string()).collect());

    if !args.skip.is_empty() {
        validate_steps(&args.skip)?;
        let skip: HashSet<&str> = args.skip.iter().map(String::as_str).collect();
        steps.retain(|s| !skip.contains(s.as_str()));
    }

    if !args.only.is_empty() {
        validate_steps(&args.only)?;
        let only: HashSet<&str> = args.only.iter().map(String::as_str).collect();
        steps.retain(|s| only.contains(s.as_str()));
    }

    async move {
        let started = Instant::now();
        let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());

        for step in &steps {
            let summary = match step.as_str() {
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
                "clean-logs" => {
                    clean_logs::run(clean_logs::Args::default(), &config.clean_logs, dry_run)
                        .await?
                }
                "clean-library-caches" => {
                    clean_library_caches::run(
                        clean_library_caches::Args::default(),
                        &config.clean_library_caches,
                        dry_run,
                    )
                    .await?
                }
                "clean-electron-caches" => {
                    clean_electron_caches::run(
                        clean_electron_caches::Args::default(),
                        &config.clean_electron_caches,
                        dry_run,
                    )
                    .await?
                }
                "clean-chrome" => {
                    clean_chrome::run(clean_chrome::Args::default(), &config.clean_chrome, dry_run)
                        .await?
                }
                "clean-steam" => {
                    clean_steam::run(clean_steam::Args::default(), &config.clean_steam, dry_run)
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
                "clean-mise" => {
                    clean_mise::run(clean_mise::Args::default(), &config.clean_mise, dry_run)
                        .await?
                }
                "clean-xdg-cache" => {
                    clean_xdg_cache::run(
                        clean_xdg_cache::Args::default(),
                        &config.clean_xdg_cache,
                        dry_run,
                    )
                    .await?
                }
                "clean-rustup" => {
                    clean_rustup::run(clean_rustup::Args::default(), &config.clean_rustup, dry_run)
                        .await?
                }
                "bz-cleanup" => {
                    bz_cleanup::run(bz_cleanup::Args::default(), &config.bz_cleanup, dry_run)
                        .await?
                }
                unknown => {
                    bail!("unknown step in `all.steps`: {unknown}");
                }
            };

            results.push(StepResult {
                name: step.clone(),
                summary,
            });
        }

        if steps.is_empty() {
            warn!("`all.steps` is empty — nothing to do");
            return Ok(());
        }

        print_aggregate_summary(&results, started.elapsed());
        Ok(())
    }
    .instrument(info_span!("all"))
    .await
}

/// Status tag for a single step: `FAIL` if anything failed, `skip` if the step
/// did no work and only recorded skips (tool absent), otherwise `ok `.
fn step_status(summary: &CommandSummary) -> &'static str {
    if !summary.passed() {
        "FAIL"
    } else if summary.skipped() {
        "skip"
    } else {
        "ok "
    }
}

/// Per-step item counts, appending `, N skipped` only when there were skips so
/// the common case stays uncluttered.
fn step_counts(summary: &CommandSummary) -> String {
    if summary.items_skipped > 0 {
        format!(
            "({} ok, {} failed, {} skipped)",
            summary.items_ok, summary.items_failed, summary.items_skipped
        )
    } else {
        format!("({} ok, {} failed)", summary.items_ok, summary.items_failed)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    passed: u64,
    failed: u64,
    skipped: u64,
}

/// Classify each step as passed / failed / skipped. A step that only recorded
/// skips counts as `skipped`, not `passed`, so an absent tool can't hide inside
/// the passed count.
fn tally(results: &[StepResult]) -> Tally {
    let mut t = Tally::default();
    for r in results {
        if !r.summary.passed() {
            t.failed += 1;
        } else if r.summary.skipped() {
            t.skipped += 1;
        } else {
            t.passed += 1;
        }
    }
    t
}

fn print_aggregate_summary(results: &[StepResult], elapsed: std::time::Duration) {
    let mut total = CommandSummary::default();
    for r in results {
        total.merge(r.summary);
    }
    let Tally {
        passed,
        failed,
        skipped,
    } = tally(results);

    info!("all: per-command results:");
    let max_name = results
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for r in results {
        let status = step_status(&r.summary);
        let padded = format!("{:<width$}", r.name, width = max_name);
        info!(
            "  [{status}] {padded}  {:>10}  {}",
            format_size(r.summary.bytes_freed, BINARY),
            step_counts(&r.summary),
        );
    }
    info!(
        "all: {passed} passed, {failed} failed, {skipped} skipped, {} freed across {} items in {}",
        format_size(total.bytes_freed, BINARY),
        total.items_total(),
        format_duration(elapsed.as_millis() as u64),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, summary: CommandSummary) -> StepResult {
        StepResult {
            name: name.to_string(),
            summary,
        }
    }

    #[test]
    fn step_status_distinguishes_skip_from_ok() {
        assert_eq!(step_status(&CommandSummary::ok_one()), "ok ");
        assert_eq!(step_status(&CommandSummary::skipped_one()), "skip");
        assert_eq!(step_status(&CommandSummary::failed_one()), "FAIL");
    }

    #[test]
    fn step_status_failure_wins_over_skip() {
        let mut s = CommandSummary::skipped_one();
        s.merge(CommandSummary::failed_one());
        assert_eq!(step_status(&s), "FAIL");
    }

    #[test]
    fn step_counts_hides_skipped_when_zero() {
        assert_eq!(step_counts(&CommandSummary::ok_one()), "(1 ok, 0 failed)");
        assert_eq!(
            step_counts(&CommandSummary::skipped_one()),
            "(0 ok, 0 failed, 1 skipped)"
        );
    }

    #[test]
    fn tally_counts_skip_separately_from_passed() {
        let results = vec![
            step("a", CommandSummary::ok_one()),
            step("b", CommandSummary::skipped_one()),
            step("c", CommandSummary::failed_one()),
            step("d", CommandSummary::ok_one()),
        ];
        assert_eq!(
            tally(&results),
            Tally {
                passed: 2,
                failed: 1,
                skipped: 1,
            }
        );
    }
}
