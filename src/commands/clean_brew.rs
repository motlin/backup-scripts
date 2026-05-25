use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use crate::config::CleanBrewConfig;

pub const DEFAULT_PRUNE: &str = "all";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Value passed to `brew cleanup --prune=<value>`. "all" also clears the
    /// download cache. [config: clean_brew.prune, default: all]
    #[arg(long)]
    prune: Option<String>,
}

pub async fn run(args: Args, cfg: &CleanBrewConfig, dry_run: bool) -> Result<()> {
    let prune = args
        .prune
        .or_else(|| cfg.prune.clone())
        .unwrap_or_else(|| DEFAULT_PRUNE.to_string());

    let span = info_span!("clean-brew", prune = %prune);
    async move {
        if !brew_available().await {
            info!("brew not on PATH — skipping");
            return Ok(());
        }

        run_brew_cleanup(&prune, dry_run).await
    }
    .instrument(span)
    .await
}

/// Returns true iff the `brew` CLI is installed and runnable.
async fn brew_available() -> bool {
    let output = Command::new("brew").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}

async fn run_brew_cleanup(prune: &str, dry_run: bool) -> Result<()> {
    let started = Instant::now();
    let prune_arg = format!("--prune={prune}");
    let mut cmd = Command::new("brew");
    cmd.arg("cleanup").arg(&prune_arg);
    if dry_run {
        cmd.arg("--dry-run");
    }

    let output = cmd
        .output()
        .await
        .context("failed to invoke `brew cleanup`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "brew cleanup failed"
        );
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let freed = parse_freed_bytes(&stdout);

    info!(
        freed = %freed
            .map(|b| format_size(b, BINARY))
            .unwrap_or_else(|| "unknown".to_string()),
        elapsed_ms = started.elapsed().as_millis() as u64,
        action = if dry_run { "would free" } else { "freed" },
        "brew cleanup complete"
    );
    Ok(())
}

/// Parse the "This operation has freed approximately X of disk space." line
/// from `brew cleanup` output. The dry-run variant says "would free" instead
/// of "has freed". Returns bytes, or None if the line is missing/unparseable
/// (e.g. when there was nothing to clean).
fn parse_freed_bytes(output: &str) -> Option<u64> {
    let line = output
        .lines()
        .find(|l| l.contains("freed approximately") || l.contains("free approximately"))?;
    // Tail looks like "... approximately 1.2GB of disk space."
    let after = line.split("approximately").nth(1)?.trim();
    let size_token = after.split_whitespace().next()?;
    parse_size(size_token)
}

/// Parse a brew-style size string like "1.2GB", "512KB", "2MB", "1.5KiB".
fn parse_size(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let split = s
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().ok()?;
    let multiplier: f64 = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        "KIB" => 1_024.0,
        "MIB" => 1_024.0 * 1_024.0,
        "GIB" => 1_024.0 * 1_024.0 * 1_024.0,
        "TIB" => 1_024.0_f64.powi(4),
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_freed_bytes_gb() {
        let out = "Removing: /Users/x/Library/Caches/Homebrew/foo--1.0\n\
                   ==> This operation has freed approximately 1.2GB of disk space.\n";
        assert_eq!(parse_freed_bytes(out), Some(1_200_000_000));
    }

    #[test]
    fn parses_freed_bytes_dry_run() {
        let out = "Would remove: /Users/x/Library/Caches/Homebrew/foo--1.0 (1.2GB)\n\
                   ==> This operation would free approximately 512KB of disk space.\n";
        assert_eq!(parse_freed_bytes(out), Some(512_000));
    }

    #[test]
    fn parses_freed_bytes_missing() {
        assert_eq!(parse_freed_bytes("nothing to clean up\n"), None);
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("1.5KB"), Some(1_500));
        assert_eq!(parse_size("2MB"), Some(2_000_000));
        assert_eq!(parse_size("3.14GB"), Some(3_140_000_000));
        assert_eq!(parse_size("nonsense"), None);
        assert_eq!(parse_size(""), None);
    }
}
