use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::PathBuf;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanGoModCacheConfig;
use crate::ui::format_duration;
use crate::walk::dir_size;

pub const DEFAULT_MINIMUM_GIBIBYTES: u64 = 5;
const BYTES_PER_GIBIBYTE: u64 = 1_073_741_824;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Actually run `go clean -modcache`. Without this flag the command previews
    /// the eligible cache. [config: `clean_go_mod_cache.remove`, default: false]
    #[arg(long)]
    remove: bool,

    /// Minimum module-cache size in GiB before cleanup is eligible.
    /// [config: `clean_go_mod_cache.minimum_gibibytes`, default: 5]
    #[arg(long)]
    minimum_gibibytes: Option<u64>,
}

pub async fn run(args: Args, cfg: &CleanGoModCacheConfig, dry_run: bool) -> Result<CommandSummary> {
    let remove = args.remove || cfg.remove.unwrap_or(false);
    let minimum_gibibytes = args
        .minimum_gibibytes
        .or(cfg.minimum_gibibytes)
        .unwrap_or(DEFAULT_MINIMUM_GIBIBYTES);
    let minimum_bytes = minimum_gibibytes.saturating_mul(BYTES_PER_GIBIBYTE);

    async move {
        if !go_available().await {
            warn!("go not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let cache = module_cache_path().await?;
        if !cache.is_dir() {
            info!("Go module cache does not exist: {}", cache.display());
            return Ok(CommandSummary::skipped_one());
        }

        let bytes = dir_size(&cache).await.unwrap_or(0);
        if bytes < minimum_bytes {
            info!(
                size = %format_size(bytes, BINARY),
                minimum = %format_size(minimum_bytes, BINARY),
                "Go module cache is below the cleanup threshold"
            );
            return Ok(CommandSummary::ok_one());
        }

        if dry_run {
            info!(
                size = %format_size(bytes, BINARY),
                "dry run: would run `go clean -modcache`"
            );
            return Ok(CommandSummary::ok_one_with_bytes(bytes));
        }
        if !remove {
            info!(
                size = %format_size(bytes, BINARY),
                "no --remove: would run `go clean -modcache`"
            );
            return Ok(CommandSummary::ok_one());
        }

        let started = Instant::now();
        let output = Command::new("go")
            .arg("clean")
            .arg("-modcache")
            .output()
            .await
            .context("failed to invoke `go clean -modcache`")?;
        if !output.status.success() {
            warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "`go clean -modcache` failed"
            );
            return Ok(CommandSummary::failed_one());
        }

        info!(
            reclaimed = %format_size(bytes, BINARY),
            elapsed = %format_duration(started.elapsed().as_millis()),
            "Go module cache cleaned"
        );
        Ok(CommandSummary::ok_one_with_bytes(bytes))
    }
    .instrument(info_span!("clean-go-mod-cache", minimum_gibibytes, remove))
    .await
}

async fn go_available() -> bool {
    let output = Command::new("go").arg("version").output().await;
    matches!(output, Ok(output) if output.status.success())
}

async fn module_cache_path() -> Result<PathBuf> {
    let output = Command::new("go")
        .arg("env")
        .arg("GOMODCACHE")
        .output()
        .await
        .context("failed to invoke `go env GOMODCACHE`")?;
    if !output.status.success() {
        bail!(
            "`go env GOMODCACHE` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_module_cache_path(&output.stdout)
}

fn parse_module_cache_path(output: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(output)
        .context("`go env GOMODCACHE` returned non-UTF-8 output")?
        .trim();
    if path.is_empty() {
        bail!("`go env GOMODCACHE` returned an empty path");
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_module_cache_path_with_spaces() {
        assert_eq!(
            parse_module_cache_path(b"/Users/alice/Go Cache/pkg/mod\n")
                .expect("module cache path is parsed"),
            PathBuf::from("/Users/alice/Go Cache/pkg/mod")
        );
    }

    #[test]
    fn rejects_empty_module_cache_path() {
        assert_eq!(
            parse_module_cache_path(b"\n")
                .expect_err("empty module cache path is rejected")
                .to_string(),
            "`go env GOMODCACHE` returned an empty path"
        );
    }
}
