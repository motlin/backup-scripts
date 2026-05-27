use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use crate::config::CleanYarnConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, _cfg: &CleanYarnConfig, dry_run: bool) -> Result<()> {
    async move {
        if !tool_available().await {
            info!("yarn not on PATH — skipping");
            return Ok(());
        }
        if dry_run {
            info!("dry run: would run `yarn cache clean`");
            return Ok(());
        }
        let started = Instant::now();
        let output = Command::new("yarn")
            .arg("cache")
            .arg("clean")
            .output()
            .await
            .context("failed to invoke `yarn cache clean`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "yarn cache clean failed"
            );
            return Ok(());
        }
        info!(
            elapsed = %format_duration(started.elapsed().as_millis() as u64),
            "yarn cache cleaned"
        );
        Ok(())
    }
    .instrument(info_span!("clean-yarn"))
    .await
}

async fn tool_available() -> bool {
    let output = Command::new("yarn").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}
