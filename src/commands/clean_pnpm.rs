use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use crate::config::CleanPnpmConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, _cfg: &CleanPnpmConfig, dry_run: bool) -> Result<()> {
    async move {
        if !tool_available().await {
            info!("pnpm not on PATH — skipping");
            return Ok(());
        }
        if dry_run {
            info!("dry run: would run `pnpm store prune`");
            return Ok(());
        }
        let started = Instant::now();
        let output = Command::new("pnpm")
            .arg("store")
            .arg("prune")
            .output()
            .await
            .context("failed to invoke `pnpm store prune`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pnpm store prune failed"
            );
            return Ok(());
        }
        info!(
            elapsed = %format_duration(started.elapsed().as_millis() as u64),
            "pnpm store pruned"
        );
        Ok(())
    }
    .instrument(info_span!("clean-pnpm"))
    .await
}

async fn tool_available() -> bool {
    let output = Command::new("pnpm").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}
