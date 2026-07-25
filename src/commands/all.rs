use anyhow::{Result, bail};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::collections::HashSet;
use std::time::Instant;
use tracing::warn;

use crate::config::AppConfig;
use crate::ui;
use crate::ui::format_duration;

use super::{
    CommandSummary, bz_cleanup, clean_brew, clean_cargo, clean_chrome, clean_cocoapods,
    clean_cypress, clean_docker, clean_electron_caches, clean_go_build, clean_go_mod_cache,
    clean_gradle, clean_jetbrains, clean_library_caches, clean_logs, clean_m2, clean_maven,
    clean_mise, clean_node, clean_node_gyp, clean_npm, clean_pip, clean_playwright, clean_pnpm,
    clean_python_artifacts, clean_rustup, clean_steam, clean_tmp, clean_trash, clean_xcode,
    clean_xdg_cache, clean_yarn, git_maintenance,
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
    "clean-go-mod-cache",
    "clean-python-artifacts",
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

/// Resolve the final, ordered step list by applying `--skip` then `--only` to the
/// configured steps. `--skip` removes named steps; `--only`, when non-empty,
/// retains only named steps. Applying both leaves `--only` minus `--skip`. Unknown
/// names in either list produce an error via `validate_steps`. Step order from the
/// configured list is preserved.
fn resolve_steps(mut steps: Vec<String>, skip: &[String], only: &[String]) -> Result<Vec<String>> {
    if !skip.is_empty() {
        validate_steps(skip)?;
        let skip: HashSet<&str> = skip.iter().map(String::as_str).collect();
        steps.retain(|s| !skip.contains(s.as_str()));
    }

    if !only.is_empty() {
        validate_steps(only)?;
        let only: HashSet<&str> = only.iter().map(String::as_str).collect();
        steps.retain(|s| only.contains(s.as_str()));
    }

    Ok(steps)
}

pub async fn run(args: Args, config: &AppConfig, dry_run: bool) -> Result<()> {
    let configured: Vec<String> = config.all.steps.clone().unwrap_or_else(|| {
        DEFAULT_STEPS
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    });

    let steps = resolve_steps(configured, &args.skip, &args.only)?;

    if steps.is_empty() {
        warn!("`all.steps` is empty — nothing to do");
        return Ok(());
    }

    let started = Instant::now();
    let (results, outcome) = run_steps(&steps, config, dry_run).await;

    // Always print the aggregate summary of completed steps, even when a
    // step failed mid-run, before propagating that failure.
    print_aggregate_summary(&results, started.elapsed(), dry_run);

    if let Some(command) = rerun_command(&results, dry_run) {
        let skipped = results.iter().filter(|r| r.summary.skipped()).count();
        ui::emit_line(&format!(
            "all: {skipped} cleaner(s) skipped. Re-run just those:"
        ));
        ui::emit_line(&format!("  {command}"));
    }

    outcome
}

/// Run each step in order, collecting a `StepResult` per completed step. On the
/// first step that returns `Err`, stop and return the results gathered so far
/// alongside the error so the caller can still print a partial summary.
async fn run_steps(
    steps: &[String],
    config: &AppConfig,
    dry_run: bool,
) -> (Vec<StepResult>, Result<()>) {
    let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());

    for step in steps {
        match run_step(step, config, dry_run).await {
            Ok(summary) => results.push(StepResult {
                name: step.clone(),
                summary,
            }),
            Err(e) => return (results, Err(e)),
        }
    }

    (results, Ok(()))
}

/// Dispatch a single step by name to its cleaner, returning that cleaner's
/// `CommandSummary`.
// A flat `match` mapping each step name to its command's `run`; every arm is a few
// lines. The length is inherent to the dispatch table.
#[allow(clippy::too_many_lines)]
async fn run_step(step: &str, config: &AppConfig, dry_run: bool) -> Result<CommandSummary> {
    let summary = match step {
        "git-maintenance" => {
            git_maintenance::run(
                git_maintenance::Args::default(),
                &config.git_maintenance,
                config.roots.as_ref(),
                dry_run,
            )
            .await?
        }
        "clean-maven" => {
            clean_maven::run(
                clean_maven::Args::default(),
                &config.clean_maven,
                config.roots.as_ref(),
                dry_run,
            )
            .await?
        }
        "clean-node" => {
            clean_node::run(
                clean_node::Args::default(),
                &config.clean_node,
                config.roots.as_ref(),
                dry_run,
            )
            .await?
        }
        "clean-cargo" => {
            clean_cargo::run(
                clean_cargo::Args::default(),
                &config.clean_cargo,
                config.roots.as_ref(),
                dry_run,
            )
            .await?
        }
        "clean-m2" => clean_m2::run(clean_m2::Args::default(), &config.clean_m2, dry_run).await?,
        "clean-gradle" => {
            clean_gradle::run(clean_gradle::Args::default(), &config.clean_gradle, dry_run).await?
        }
        "clean-tmp" => {
            clean_tmp::run(clean_tmp::Args::default(), &config.clean_tmp, dry_run).await?
        }
        "clean-xcode" => {
            clean_xcode::run(clean_xcode::Args::default(), &config.clean_xcode, dry_run).await?
        }
        "clean-docker" => {
            clean_docker::run(clean_docker::Args::default(), &config.clean_docker, dry_run).await?
        }
        "clean-brew" => {
            clean_brew::run(clean_brew::Args::default(), &config.clean_brew, dry_run).await?
        }
        "clean-npm" => {
            clean_npm::run(clean_npm::Args::default(), &config.clean_npm, dry_run).await?
        }
        "clean-trash" => {
            clean_trash::run(clean_trash::Args::default(), &config.clean_trash, dry_run).await?
        }
        "clean-yarn" => {
            clean_yarn::run(clean_yarn::Args::default(), &config.clean_yarn, dry_run).await?
        }
        "clean-pnpm" => {
            clean_pnpm::run(clean_pnpm::Args::default(), &config.clean_pnpm, dry_run).await?
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
        "clean-go-mod-cache" => {
            clean_go_mod_cache::run(
                clean_go_mod_cache::Args::default(),
                &config.clean_go_mod_cache,
                dry_run,
            )
            .await?
        }
        "clean-python-artifacts" => {
            clean_python_artifacts::run(
                clean_python_artifacts::Args::default(),
                &config.clean_python_artifacts,
                config.roots.as_ref(),
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
            clean_logs::run(clean_logs::Args::default(), &config.clean_logs, dry_run).await?
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
            clean_chrome::run(clean_chrome::Args::default(), &config.clean_chrome, dry_run).await?
        }
        "clean-steam" => {
            clean_steam::run(clean_steam::Args::default(), &config.clean_steam, dry_run).await?
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
            clean_mise::run(clean_mise::Args::default(), &config.clean_mise, dry_run).await?
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
            clean_rustup::run(
                clean_rustup::Args::default(),
                &config.clean_rustup,
                config.roots.as_ref(),
                dry_run,
            )
            .await?
        }
        "bz-cleanup" => {
            bz_cleanup::run(bz_cleanup::Args::default(), &config.bz_cleanup, dry_run).await?
        }
        unknown => {
            bail!("unknown step in `all.steps`: {unknown}");
        }
    };

    Ok(summary)
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

/// Header for the per-command results table, noting dry-run mode so the bare
/// size column isn't read as already-freed bytes.
fn results_header(dry_run: bool) -> &'static str {
    if dry_run {
        "all: per-command results (dry run):"
    } else {
        "all: per-command results:"
    }
}

/// Render the final aggregate line, using `would free` in dry-run mode so the
/// summary can't be mistaken for actual deletions.
fn aggregate_line(results: &[StepResult], elapsed: std::time::Duration, dry_run: bool) -> String {
    let mut total = CommandSummary::default();
    for r in results {
        total.merge(r.summary);
    }
    let Tally {
        passed,
        failed,
        skipped,
    } = tally(results);

    let verb = if dry_run { "would free" } else { "freed" };
    format!(
        "all: {passed} passed, {failed} failed, {skipped} skipped, {verb} {} across {} items in {}",
        format_size(total.bytes_freed, BINARY),
        total.items_total(),
        format_duration(elapsed.as_millis()),
    )
}

/// Print the aggregate summary through `ui::emit_line` (like the per-command
/// trees) rather than `tracing`, so the block renders as plain lines without the
/// hierarchical layer's span connectors.
fn print_aggregate_summary(results: &[StepResult], elapsed: std::time::Duration, dry_run: bool) {
    ui::emit_line("");
    ui::emit_line(results_header(dry_run));
    let max_name = results
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    for r in results {
        let status = step_status(&r.summary);
        let padded = format!("{:<width$}", r.name, width = max_name);
        ui::emit_line(&format!(
            "  [{status}] {padded}  {:>10}  {}",
            format_size(r.summary.bytes_freed, BINARY),
            step_counts(&r.summary),
        ));
    }
    ui::emit_line(&aggregate_line(results, elapsed, dry_run));
}

/// Build the `backup all --only …` command that re-runs exactly the steps that
/// were skipped at runtime (`summary.skipped()`), in step order. Returns `None`
/// when nothing was skipped. Includes `--dry-run` when the original run was a
/// dry run.
fn rerun_command(results: &[StepResult], dry_run: bool) -> Option<String> {
    let only_args: Vec<String> = results
        .iter()
        .filter(|r| r.summary.skipped())
        .map(|r| format!("--only {}", r.name))
        .collect();

    if only_args.is_empty() {
        return None;
    }

    let dry = if dry_run { " --dry-run" } else { "" };
    Some(format!("backup all{dry} {}", only_args.join(" ")))
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
    fn results_header_notes_dry_run() {
        assert_eq!(results_header(false), "all: per-command results:");
        assert_eq!(results_header(true), "all: per-command results (dry run):");
    }

    #[test]
    fn aggregate_line_dry_run_says_would_free() {
        let results = vec![step("a", CommandSummary::ok_one_with_bytes(1024))];
        let line = aggregate_line(&results, std::time::Duration::from_millis(1200), true);
        assert_eq!(
            line,
            "all: 1 passed, 0 failed, 0 skipped, would free 1 KiB across 1 items in 1.2s"
        );
    }

    #[test]
    fn aggregate_line_real_run_says_freed() {
        let results = vec![step("a", CommandSummary::ok_one_with_bytes(1024))];
        let line = aggregate_line(&results, std::time::Duration::from_millis(1200), false);
        assert_eq!(
            line,
            "all: 1 passed, 0 failed, 0 skipped, freed 1 KiB across 1 items in 1.2s"
        );
    }

    #[test]
    fn step_counts_hides_skipped_when_zero() {
        assert_eq!(step_counts(&CommandSummary::ok_one()), "(1 ok, 0 failed)");
        assert_eq!(
            step_counts(&CommandSummary::skipped_one()),
            "(0 ok, 0 failed, 1 skipped)"
        );
    }

    fn steps_vec(names: &[&str]) -> Vec<String> {
        names.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn resolve_steps_only_retains_named_steps_in_order() {
        let configured = steps_vec(&["clean-node", "clean-chrome", "clean-steam", "clean-tmp"]);
        let only = steps_vec(&["clean-tmp", "clean-node"]);

        let resolved = resolve_steps(configured, &[], &only).unwrap();

        // Only the named steps survive, and the configured order is preserved
        // (not the order the `--only` flags were given in).
        assert_eq!(resolved, steps_vec(&["clean-node", "clean-tmp"]));
    }

    #[test]
    fn resolve_steps_unknown_only_name_errors() {
        let configured = steps_vec(&["clean-node"]);
        let only = steps_vec(&["not-a-real-step"]);

        let err = resolve_steps(configured, &[], &only).unwrap_err();

        assert!(
            err.to_string().contains("not-a-real-step"),
            "error should name the unknown step",
        );
    }

    #[test]
    fn resolve_steps_only_minus_skip() {
        let configured = steps_vec(&["clean-node", "clean-chrome", "clean-steam"]);
        let only = steps_vec(&["clean-node", "clean-chrome"]);
        let skip = steps_vec(&["clean-chrome"]);

        let resolved = resolve_steps(configured, &skip, &only).unwrap();

        // `--only` selects {clean-node, clean-chrome}; `--skip` removes clean-chrome,
        // leaving only clean-node.
        assert_eq!(resolved, steps_vec(&["clean-node"]));
    }

    #[tokio::test]
    async fn run_steps_returns_partial_results_with_error_on_failure() {
        // An unknown step name makes `run_step` bail. `run_steps` must return the
        // results gathered before the failure (here, none) alongside the error,
        // rather than discarding them — so the caller can still print a partial
        // summary.
        let config = AppConfig::default();
        let steps = vec!["not-a-real-step".to_string()];

        let (results, outcome) = run_steps(&steps, &config, true).await;

        assert!(results.is_empty());
        assert!(outcome.is_err());
        assert!(
            outcome.unwrap_err().to_string().contains("not-a-real-step"),
            "error should name the offending step",
        );
    }

    #[test]
    fn rerun_command_is_none_when_nothing_skipped() {
        let results = vec![
            step("a", CommandSummary::ok_one()),
            step("b", CommandSummary::failed_one()),
        ];
        assert_eq!(rerun_command(&results, false), None);
    }

    #[test]
    fn rerun_command_lists_skipped_steps_in_order() {
        let results = vec![
            step("clean-chrome", CommandSummary::skipped_one()),
            step("clean-node", CommandSummary::ok_one()),
            step("clean-steam", CommandSummary::skipped_one()),
        ];
        assert_eq!(
            rerun_command(&results, false),
            Some("backup all --only clean-chrome --only clean-steam".to_string())
        );
    }

    #[test]
    fn rerun_command_includes_dry_run_flag() {
        let results = vec![step("clean-chrome", CommandSummary::skipped_one())];
        assert_eq!(
            rerun_command(&results, true),
            Some("backup all --dry-run --only clean-chrome".to_string())
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
