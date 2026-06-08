use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanGoBuildConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, _cfg: &CleanGoBuildConfig, dry_run: bool) -> Result<CommandSummary> {
    async move {
        if !tool_available().await {
            warn!("go not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }
        let started = Instant::now();
        let mut cmd = Command::new("go");
        cmd.arg("clean").arg("-cache");
        if dry_run {
            cmd.arg("-n");
        }
        let output = cmd
            .output()
            .await
            .context("failed to invoke `go clean -cache`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "go clean -cache failed"
            );
            return Ok(CommandSummary::failed_one());
        }
        let verb = if dry_run { "would clean" } else { "cleaned" };
        info!(
            elapsed = %format_duration(started.elapsed().as_millis()),
            "go build cache {verb}"
        );
        Ok(CommandSummary::ok_one())
    }
    .instrument(info_span!("clean-go-build"))
    .await
}

async fn tool_available() -> bool {
    let output = Command::new("go").arg("version").output().await;
    matches!(output, Ok(out) if out.status.success())
}
