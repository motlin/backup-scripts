use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanGemsConfig;
use crate::ui::format_duration;
use crate::walk::dir_size;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Actually remove obsolete user-installed gem versions. Without this flag
    /// `RubyGems` runs in dry-run mode. [config: `clean_gems.remove`, default: false]
    #[arg(long)]
    remove: bool,
}

pub async fn run(args: Args, cfg: &CleanGemsConfig, dry_run: bool) -> Result<CommandSummary> {
    let remove = args.remove || cfg.remove.unwrap_or(false);

    async move {
        if !gem_available().await {
            warn!("gem not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let gem_home = user_gem_home().await?;
        if !gem_home.is_dir() {
            info!(
                path = %gem_home.display(),
                "user gem directory does not exist"
            );
            return Ok(CommandSummary::skipped_one());
        }

        let preview = dry_run || !remove;
        let before = dir_size(&gem_home).await.unwrap_or(0);
        let started = Instant::now();
        let mut command = Command::new("gem");
        command.arg("cleanup").arg("--user-install");
        if preview {
            command.arg("--dry-run");
        }
        let output = command
            .output()
            .await
            .context("failed to invoke `gem cleanup --user-install`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "`gem cleanup --user-install` failed"
            );
            return Ok(CommandSummary::failed_one());
        }

        if preview {
            let reason = if dry_run { "dry run" } else { "no --remove" };
            info!(
                size = %format_size(before, BINARY),
                elapsed = %format_duration(started.elapsed().as_millis()),
                "{reason}: RubyGems preview completed"
            );
            return Ok(CommandSummary::ok_one());
        }

        let after = dir_size(&gem_home).await.unwrap_or(0);
        let reclaimed = before.saturating_sub(after);
        info!(
            reclaimed = %format_size(reclaimed, BINARY),
            elapsed = %format_duration(started.elapsed().as_millis()),
            "obsolete user-installed gem versions cleaned"
        );
        Ok(CommandSummary::ok_one_with_bytes(reclaimed))
    }
    .instrument(info_span!("clean-gems", remove))
    .await
}

async fn gem_available() -> bool {
    let output = Command::new("gem").arg("--version").output().await;
    matches!(output, Ok(output) if output.status.success())
}

async fn user_gem_home() -> Result<PathBuf> {
    let output = Command::new("gem")
        .arg("environment")
        .arg("user_gemhome")
        .output()
        .await
        .context("failed to invoke `gem environment user_gemhome`")?;
    if !output.status.success() {
        bail!(
            "`gem environment user_gemhome` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_user_gem_home(&output.stdout)
}

fn parse_user_gem_home(output: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(output)
        .context("`gem environment user_gemhome` returned non-UTF-8 output")?
        .trim();
    if path.is_empty() {
        bail!("`gem environment user_gemhome` returned an empty path");
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_gem_home_with_spaces() {
        assert_eq!(
            parse_user_gem_home(b"/Users/alice/Ruby Gems/4.0.0\n")
                .expect("user gem home is parsed"),
            PathBuf::from("/Users/alice/Ruby Gems/4.0.0")
        );
    }

    #[test]
    fn rejects_empty_user_gem_home() {
        assert_eq!(
            parse_user_gem_home(b"\n")
                .expect_err("empty user gem home is rejected")
                .to_string(),
            "`gem environment user_gemhome` returned an empty path"
        );
    }
}
