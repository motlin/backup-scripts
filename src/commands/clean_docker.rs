use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanDockerConfig;

pub const DEFAULT_HOURS: u32 = 720;
pub const DEFAULT_SCOPE: &str = "system";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Only prune objects older than this many hours. [config: clean_docker.hours, default: 720]
    #[arg(long)]
    hours: Option<u32>,

    /// Prune scope. Currently only "system" is supported (covers images, containers,
    /// volumes, build cache, networks). [config: clean_docker.scope, default: system]
    #[arg(long)]
    scope: Option<String>,
}

pub async fn run(args: Args, cfg: &CleanDockerConfig, dry_run: bool) -> Result<CommandSummary> {
    let hours = args.hours.or(cfg.hours).unwrap_or(DEFAULT_HOURS);
    let scope = args
        .scope
        .or_else(|| cfg.scope.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_string());

    let span = info_span!("clean-docker", hours, scope = %scope);
    async move {
        if !docker_available().await {
            info!("docker CLI not installed or daemon not running — skipping");
            return Ok(CommandSummary::default());
        }

        if scope != "system" {
            warn!(scope = %scope, "unsupported scope; only \"system\" is implemented");
            return Ok(CommandSummary::failed_one());
        }

        if dry_run {
            run_df().await
        } else {
            run_prune(hours).await
        }
    }
    .instrument(span)
    .await
}

/// Returns true iff the `docker` CLI is installed AND the daemon responds.
/// We use `docker version --format {{.Server.Version}}` because it fails fast
/// (and with a non-zero status) when the daemon is unreachable, unlike `docker --version`
/// which only reports the client version.
async fn docker_available() -> bool {
    let output = Command::new("docker")
        .arg("version")
        .arg("--format")
        .arg("{{.Server.Version}}")
        .output()
        .await;
    matches!(output, Ok(out) if out.status.success())
}

async fn run_df() -> Result<CommandSummary> {
    let started = Instant::now();
    let output = Command::new("docker")
        .arg("system")
        .arg("df")
        .output()
        .await
        .context("failed to invoke `docker system df`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "docker system df failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reclaimable = parse_reclaimable_total(&stdout);

    for line in stdout.lines() {
        info!("{line}");
    }

    info!(
        reclaimable = %reclaimable
            .map(|b| format_size(b, BINARY))
            .unwrap_or_else(|| "unknown".to_string()),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "dry run: would prune objects shown above"
    );
    Ok(CommandSummary::ok_one_with_bytes(reclaimable.unwrap_or(0)))
}

async fn run_prune(hours: u32) -> Result<CommandSummary> {
    let started = Instant::now();
    let filter = format!("until={hours}h");
    let output = Command::new("docker")
        .arg("system")
        .arg("prune")
        .arg("-f")
        .arg("--filter")
        .arg(&filter)
        .output()
        .await
        .context("failed to invoke `docker system prune`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "docker system prune failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reclaimed = parse_total_reclaimed(&stdout);

    info!(
        reclaimed = %reclaimed
            .map(|b| format_size(b, BINARY))
            .unwrap_or_else(|| "unknown".to_string()),
        filter = %filter,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "pruned"
    );
    Ok(CommandSummary::ok_one_with_bytes(reclaimed.unwrap_or(0)))
}

/// Parse the `Total reclaimed space: X.YGB` line from `docker system prune` output.
/// Returns bytes, or None if the line is missing/unparseable.
fn parse_total_reclaimed(output: &str) -> Option<u64> {
    let line = output.lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("Total reclaimed space:").map(str::trim)
    })?;
    parse_size(line)
}

/// Sum the RECLAIMABLE column from `docker system df` output.
///
/// Expected layout (whitespace-separated):
///   TYPE            TOTAL  ACTIVE  SIZE    RECLAIMABLE
///   Images          14     7       13.74GB 9.209GB (67%)
///
/// "TYPE" can be one or two words ("Local Volumes", "Build Cache"). RECLAIMABLE
/// is always the first whitespace-token after SIZE — we count from the right since
/// the trailing "(NN%)" is optional (Build Cache rows omit it).
fn parse_reclaimable_total(output: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for line in output.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        // We need at least 5 columns to reach RECLAIMABLE; trailing "(NN%)" may add a 6th.
        if cols.len() < 5 {
            continue;
        }
        let reclaim = if cols.last().is_some_and(|c| c.starts_with('(')) {
            cols[cols.len() - 2]
        } else {
            cols[cols.len() - 1]
        };
        if let Some(bytes) = parse_size(reclaim) {
            total = total.saturating_add(bytes);
            any = true;
        }
    }
    any.then_some(total)
}

/// Parse a docker-style size string like "9.209GB", "18.44MB", "0B", "1.5kB".
fn parse_size(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Split numeric prefix from unit suffix.
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
    fn parses_total_reclaimed_basic() {
        let out = "Deleted Containers:\nabc\n\nTotal reclaimed space: 1.5GB\n";
        assert_eq!(parse_total_reclaimed(out), Some(1_500_000_000));
    }

    #[test]
    fn parses_total_reclaimed_zero() {
        assert_eq!(
            parse_total_reclaimed("Total reclaimed space: 0B\n"),
            Some(0)
        );
    }

    #[test]
    fn parses_total_reclaimed_missing() {
        assert_eq!(parse_total_reclaimed("nothing to see\n"), None);
    }

    #[test]
    fn parses_reclaimable_total_typical() {
        let out = "\
TYPE            TOTAL     ACTIVE    SIZE      RECLAIMABLE
Images          14        7         13.74GB   9.209GB (67%)
Containers      7         7         18.44MB   0B (0%)
Local Volumes   7         7         3.81GB    0B (0%)
Build Cache     60        0         8.069GB   8.069GB
";
        let total = parse_reclaimable_total(out).expect("parsed");
        // 9.209 GB + 0 + 0 + 8.069 GB ~= 17.278 GB
        let expected = 9_209_000_000u64 + 8_069_000_000u64;
        let diff = total.abs_diff(expected);
        assert!(diff < 10_000_000, "got {total}, expected ~{expected}");
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("100B"), Some(100));
        assert_eq!(parse_size("1.5kB"), Some(1_500));
        assert_eq!(parse_size("2MB"), Some(2_000_000));
        assert_eq!(parse_size("3.14GB"), Some(3_140_000_000));
        assert_eq!(parse_size("nonsense"), None);
        assert_eq!(parse_size(""), None);
    }
}
