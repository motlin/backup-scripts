use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanMiseConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

/// Reclaim disk under the mise data/cache dirs without removing in-use tools.
///
/// Two operations, both safe by design:
///   * `mise cache prune` — age-based GC of stale metadata under the cache dir.
///     Preferred over `mise cache clear`, since <http:/npm>: backends symlink
///     installs into the cache (jdx/mise#7267) and a blanket clear would break
///     them.
///   * `mise prune` — removes dead tracked-config links and tool VERSIONS no
///     longer referenced by any remaining config. Versions pinned by a config
///     (incl. this repo's `.mise/config.toml`) are kept.
///
/// We never touch `~/.local/share/mise/installs` directly.
pub async fn run(_args: Args, _cfg: &CleanMiseConfig, dry_run: bool) -> Result<CommandSummary> {
    async move {
        if !mise_available().await {
            warn!("mise not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let started = Instant::now();
        let mut summary = CommandSummary::default();

        summary.merge(run_cache_prune(dry_run).await?);

        let (prune_summary, pruned) = run_prune(dry_run).await?;
        summary.merge(prune_summary);

        info!(
            elapsed = %format_duration(started.elapsed().as_millis()),
            "{} {pruned} tool version(s)",
            if dry_run { "dry run: would prune" } else { "pruned" },
        );
        Ok(summary)
    }
    .instrument(info_span!("clean-mise"))
    .await
}

/// Returns true iff the `mise` CLI is installed and runnable.
async fn mise_available() -> bool {
    let output = Command::new("mise").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}

/// `mise cache prune` — age-based GC of stale cache metadata. The cache holds no
/// version-pinned installs we care about counting, so this contributes only a
/// pass/fail to the summary.
async fn run_cache_prune(dry_run: bool) -> Result<CommandSummary> {
    let mut cmd = Command::new("mise");
    cmd.arg("cache").arg("prune");
    if dry_run {
        cmd.arg("--dry-run");
    }

    let output = cmd
        .output()
        .await
        .context("failed to invoke `mise cache prune`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "mise cache prune failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    if dry_run {
        info!("dry run: `mise cache prune` would GC stale cache metadata");
    } else {
        info!("`mise cache prune` removed stale cache metadata");
    }
    Ok(CommandSummary::ok_one())
}

/// `mise prune` — removes dead tracked-config links and unused tool versions.
/// Returns the run summary plus the number of versions pruned (so the caller can
/// report it without conflating it with the cache-prune pass). Bytes freed are
/// not reported by mise, so the summary carries item counts only.
async fn run_prune(dry_run: bool) -> Result<(CommandSummary, u64)> {
    let mut cmd = Command::new("mise");
    cmd.arg("prune");
    if dry_run {
        cmd.arg("--dry-run");
    }

    let output = cmd
        .output()
        .await
        .context("failed to invoke `mise prune`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "mise prune failed"
        );
        return Ok((CommandSummary::failed_one(), 0));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let count = count_pruned_versions(&stdout);

    if dry_run {
        info!("dry run: `mise prune` would remove {count} unused tool version(s)");
    } else {
        info!("`mise prune` removed {count} unused tool version(s)");
    }

    // One ok per pruned version; a no-op run still records a single ok so it
    // counts as success rather than a no-op.
    let summary = CommandSummary {
        items_ok: count.max(1),
        ..CommandSummary::default()
    };
    Ok((summary, count))
}

/// Count the tool versions `mise prune` removed (or would remove). mise emits one
/// `✓ done` line per pruned version, e.g.
///   `mise just@1.48.0 [dryrun]      ✓ done`
/// The `✓ done` marker is per-tool and excludes the configuration-links line,
/// making it a stable count regardless of dry-run formatting.
fn count_pruned_versions(output: &str) -> u64 {
    output
        .lines()
        .filter(|l| l.contains("✓ done"))
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_done_lines() {
        let out = "\
mise pruned configuration links [dryrun]
mise java@temurin-17.0.18+8 [dryrun]  uninstall
mise java@temurin-17.0.18+8 [dryrun]  remove ~/.local/share/mise/installs/java/temurin-17.0.18+8
mise java@temurin-17.0.18+8 [dryrun]  ✓ done
mise just@1.48.0 [dryrun]        uninstall
mise just@1.48.0 [dryrun]        remove ~/.local/share/mise/installs/just/1.48.0
mise just@1.48.0 [dryrun]      ✓ done
mise node@24.16.0 [dryrun]       uninstall
mise node@24.16.0 [dryrun]     ✓ done
";
        assert_eq!(count_pruned_versions(out), 3);
    }

    #[test]
    fn counts_zero_when_nothing_pruned() {
        let out = "mise pruned configuration links\n";
        assert_eq!(count_pruned_versions(out), 0);
    }

    #[test]
    fn counts_zero_on_empty() {
        assert_eq!(count_pruned_versions(""), 0);
    }

    #[test]
    fn ignores_config_links_line_without_done_marker() {
        let out = "mise pruned configuration links [dryrun]\n";
        assert_eq!(count_pruned_versions(out), 0);
    }
}
