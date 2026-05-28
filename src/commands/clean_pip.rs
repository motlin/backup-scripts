use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanPipConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, _cfg: &CleanPipConfig, dry_run: bool) -> Result<CommandSummary> {
    async move {
        let invocation = match detect_pip().await {
            Some(inv) => inv,
            None => {
                info!("pip not on PATH (tried `pip`, `python3 -m pip`) — skipping");
                return Ok(CommandSummary::default());
            }
        };
        if dry_run {
            info!("dry run: would run `{} cache purge`", invocation.display());
            return Ok(CommandSummary::ok_one());
        }
        let started = Instant::now();
        let output = invocation
            .build_command()
            .arg("cache")
            .arg("purge")
            .output()
            .await
            .with_context(|| format!("failed to invoke `{} cache purge`", invocation.display()))?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pip cache purge failed"
            );
            return Ok(CommandSummary::failed_one());
        }
        info!(
            elapsed = %format_duration(started.elapsed().as_millis() as u64),
            "pip cache purged"
        );
        Ok(CommandSummary::ok_one())
    }
    .instrument(info_span!("clean-pip"))
    .await
}

enum PipInvocation {
    Direct,
    PythonModule,
}

impl PipInvocation {
    fn build_command(&self) -> Command {
        match self {
            Self::Direct => Command::new("pip"),
            Self::PythonModule => {
                let mut c = Command::new("python3");
                c.arg("-m").arg("pip");
                c
            }
        }
    }

    fn display(&self) -> &'static str {
        match self {
            Self::Direct => "pip",
            Self::PythonModule => "python3 -m pip",
        }
    }
}

async fn detect_pip() -> Option<PipInvocation> {
    if matches!(Command::new("pip").arg("--version").output().await, Ok(out) if out.status.success())
    {
        return Some(PipInvocation::Direct);
    }
    if matches!(
        Command::new("python3").arg("-m").arg("pip").arg("--version").output().await,
        Ok(out) if out.status.success()
    ) {
        return Some(PipInvocation::PythonModule);
    }
    None
}
