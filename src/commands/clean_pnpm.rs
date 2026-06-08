use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanPnpmConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

/// How to reach the `pnpm` binary. mise-managed tools aren't on a bare `PATH`
/// (no global version is pinned), so cron runs fall back to `mise exec`.
enum Pnpm {
    Direct,
    Mise(String),
}

impl Pnpm {
    /// A `Command` that runs `pnpm` with whatever args the caller appends.
    fn command(&self) -> Command {
        match self {
            Pnpm::Direct => Command::new("pnpm"),
            Pnpm::Mise(version) => {
                let mut cmd = Command::new("mise");
                cmd.arg("exec")
                    .arg(format!("pnpm@{version}"))
                    .arg("--")
                    .arg("pnpm");
                cmd
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Pnpm::Direct => "pnpm".to_string(),
            Pnpm::Mise(version) => format!("mise exec pnpm@{version}"),
        }
    }
}

pub async fn run(_args: Args, _cfg: &CleanPnpmConfig, dry_run: bool) -> Result<CommandSummary> {
    async move {
        let Some(pnpm) = detect_pnpm().await else {
            warn!("pnpm not found (not on PATH, no mise-installed version) — skipping");
            return Ok(CommandSummary::skipped_one());
        };
        if dry_run {
            info!("dry run: would run `{} store prune`", pnpm.describe());
            return Ok(CommandSummary::ok_one());
        }
        let started = Instant::now();
        let output = pnpm
            .command()
            .arg("store")
            .arg("prune")
            .output()
            .await
            .with_context(|| format!("failed to invoke `{} store prune`", pnpm.describe()))?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pnpm store prune failed"
            );
            return Ok(CommandSummary::failed_one());
        }
        info!(
            via = %pnpm.describe(),
            elapsed = %format_duration(started.elapsed().as_millis()),
            "pnpm store pruned"
        );
        Ok(CommandSummary::ok_one())
    }
    .instrument(info_span!("clean-pnpm"))
    .await
}

/// Prefer `pnpm` straight off `PATH`; otherwise fall back to the newest
/// mise-installed version. Returns `None` only when neither is reachable.
async fn detect_pnpm() -> Option<Pnpm> {
    if direct_pnpm_available().await {
        return Some(Pnpm::Direct);
    }
    mise_latest_pnpm().await.map(Pnpm::Mise)
}

async fn direct_pnpm_available() -> bool {
    let output = Command::new("pnpm").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}

async fn mise_latest_pnpm() -> Option<String> {
    let output = Command::new("mise")
        .arg("ls")
        .arg("--installed")
        .arg("pnpm")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    latest_installed_version(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `mise ls --installed pnpm` output (`pnpm  <version>` per line) and
/// return the highest version, compared by numeric component.
fn latest_installed_version(mise_ls: &str) -> Option<String> {
    mise_ls
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
        .map(str::to_string)
}

fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_installed_version_picks_highest() {
        let out = "pnpm  10.33.2\npnpm  11.1.1\npnpm  11.1.3\n";
        assert_eq!(latest_installed_version(out).as_deref(), Some("11.1.3"));
    }

    #[test]
    fn latest_installed_version_compares_numerically_not_lexically() {
        let out = "pnpm  9.15.0\npnpm  10.0.0\n";
        assert_eq!(latest_installed_version(out).as_deref(), Some("10.0.0"));
    }

    #[test]
    fn latest_installed_version_handles_shorter_versions() {
        let out = "pnpm  11.1\npnpm  11.1.3\n";
        assert_eq!(latest_installed_version(out).as_deref(), Some("11.1.3"));
    }

    #[test]
    fn latest_installed_version_empty_is_none() {
        assert_eq!(latest_installed_version(""), None);
        assert_eq!(latest_installed_version("\n  \n"), None);
    }
}
