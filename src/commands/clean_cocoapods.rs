use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanCocoaPodsConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(
    _args: Args,
    _cfg: &CleanCocoaPodsConfig,
    dry_run: bool,
) -> Result<CommandSummary> {
    async move {
        if !tool_available().await {
            warn!("pod not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }
        if dry_run {
            info!("dry run: would run `pod cache clean --all`");
            return Ok(CommandSummary::ok_one());
        }
        let started = Instant::now();
        let output = Command::new("pod")
            .arg("cache")
            .arg("clean")
            .arg("--all")
            .output()
            .await
            .context("failed to invoke `pod cache clean --all`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pod cache clean failed"
            );
            return Ok(CommandSummary::failed_one());
        }
        info!(
            elapsed = %format_duration(started.elapsed().as_millis() as u64),
            "CocoaPods cache cleaned"
        );
        Ok(CommandSummary::ok_one())
    }
    .instrument(info_span!("clean-cocoapods"))
    .await
}

async fn tool_available() -> bool {
    let output = Command::new("pod").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}
