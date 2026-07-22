use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::{CleanDockerConfig, expand_tilde};

pub const DEFAULT_HOURS: u32 = 720;
pub const DEFAULT_SCOPE: &str = "system";
pub const DEFAULT_ALL_IMAGES: bool = true;
pub const DEFAULT_VOLUMES: bool = false;
pub const DEFAULT_RECLAIM_PHYSICAL: bool = true;
pub const DEFAULT_DISK_IMAGE: &str =
    "~/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw";
const AUTOMATIC_RECLAIM_WAIT: Duration = Duration::from_secs(5);
const HELPER_RECLAIM_WAIT: Duration = Duration::from_secs(2);

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Only prune objects older than this many hours. [config: `clean_docker.hours`, default: 720]
    #[arg(long)]
    hours: Option<u32>,

    /// Prune scope. Currently only "system" is supported. Volumes are a separate opt-in.
    /// [config: `clean_docker.scope`, default: system]
    #[arg(long)]
    scope: Option<String>,

    /// Remove all unused images, including tagged images. [config: `clean_docker.all_images`, default: true]
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    all_images: Option<bool>,

    /// Remove all unused volumes regardless of age. [config: `clean_docker.volumes`, default: false]
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    volumes: Option<bool>,

    /// Run Docker Desktop's reclaim helper if automatic physical reclamation stalls.
    /// [config: `clean_docker.reclaim_physical`, default: true]
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    reclaim_physical: Option<bool>,

    /// Docker Desktop sparse disk image to measure. [config: `clean_docker.disk_image`]
    #[arg(long)]
    disk_image: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
struct Settings {
    hours: u32,
    scope: String,
    all_images: bool,
    volumes: bool,
    reclaim_physical: bool,
    disk_image: PathBuf,
}

#[derive(Debug)]
struct PruneResult {
    summary: CommandSummary,
    reclaimed: Option<u64>,
}

pub async fn run(args: Args, cfg: &CleanDockerConfig, dry_run: bool) -> Result<CommandSummary> {
    let settings = resolve_settings(args, cfg);

    let span = info_span!(
        "clean-docker",
        hours = settings.hours,
        scope = %settings.scope,
        all_images = settings.all_images,
        volumes = settings.volumes,
        reclaim_physical = settings.reclaim_physical,
    );
    async move {
        if !docker_available().await {
            warn!("docker CLI not installed or daemon not running — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        if settings.scope != "system" {
            warn!(scope = %settings.scope, "unsupported scope; only \"system\" is implemented");
            return Ok(CommandSummary::failed_one());
        }

        if dry_run {
            run_df(&settings).await
        } else {
            run_prune(&settings).await
        }
    }
    .instrument(span)
    .await
}

fn resolve_settings(args: Args, cfg: &CleanDockerConfig) -> Settings {
    Settings {
        hours: args.hours.or(cfg.hours).unwrap_or(DEFAULT_HOURS),
        scope: args
            .scope
            .or_else(|| cfg.scope.clone())
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string()),
        all_images: args
            .all_images
            .or(cfg.all_images)
            .unwrap_or(DEFAULT_ALL_IMAGES),
        volumes: args.volumes.or(cfg.volumes).unwrap_or(DEFAULT_VOLUMES),
        reclaim_physical: args
            .reclaim_physical
            .or(cfg.reclaim_physical)
            .unwrap_or(DEFAULT_RECLAIM_PHYSICAL),
        disk_image: expand_tilde(
            &args
                .disk_image
                .or_else(|| cfg.disk_image.clone())
                .unwrap_or_else(|| PathBuf::from(DEFAULT_DISK_IMAGE)),
        ),
    }
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

/// Reports Docker's reclaimable total, including objects excluded by prune filters.
async fn run_df(settings: &Settings) -> Result<CommandSummary> {
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

    let allocation = allocated_bytes(&settings.disk_image)?;
    warn!(
        hours = settings.hours,
        volumes = settings.volumes,
        "estimate includes all ages and volumes"
    );
    info!(
        reclaimable = %reclaimable.map_or_else(|| "unknown".to_string(), |b| format_size(b, BINARY)),
        physical_allocation = %display_size(allocation),
        disk_image = %settings.disk_image.display(),
        elapsed_ms = started.elapsed().as_millis(),
        "dry run: upper-bound estimate"
    );
    Ok(CommandSummary::ok_one_with_bytes(reclaimable.unwrap_or(0)))
}

async fn run_prune(settings: &Settings) -> Result<CommandSummary> {
    let started = Instant::now();
    let before = allocated_bytes(&settings.disk_image)?;
    let system = run_system_prune(settings).await?;
    let mut summary = system.summary;
    if !system.summary.passed() {
        return Ok(summary);
    }

    let mut docker_reclaimed = system.reclaimed;
    if settings.volumes {
        let volumes = run_volume_prune().await?;
        summary.merge(volumes.summary);
        docker_reclaimed = sum_optional_bytes(docker_reclaimed, volumes.reclaimed);
    }

    if docker_reclaimed.is_some_and(|bytes| bytes > 0) && before.is_some() {
        sleep(AUTOMATIC_RECLAIM_WAIT).await;
    }
    let after_automatic = allocated_bytes(&settings.disk_image)?;

    let mut after = after_automatic;
    if settings.reclaim_physical
        && should_run_reclaim_helper(docker_reclaimed, before, after_automatic)
    {
        let helper = run_reclaim_helper().await?;
        summary.merge(helper);
        if helper.passed() {
            sleep(HELPER_RECLAIM_WAIT).await;
            after = allocated_bytes(&settings.disk_image)?;
        }
    }

    let physical_reclaimed = physical_bytes_freed(before, after);
    summary.bytes_freed = physical_reclaimed.or(docker_reclaimed).unwrap_or(0);

    info!(
        docker_reclaimed = %display_size(docker_reclaimed),
        physical_before = %display_size(before),
        physical_after = %display_size(after),
        physical_reclaimed = %display_size(physical_reclaimed),
        disk_image = %settings.disk_image.display(),
        elapsed_ms = started.elapsed().as_millis(),
        "pruned"
    );
    Ok(summary)
}

async fn run_system_prune(settings: &Settings) -> Result<PruneResult> {
    let output = Command::new("docker")
        .args(system_prune_arguments(settings))
        .output()
        .await
        .context("failed to invoke `docker system prune`")?;
    Ok(prune_result(&output, "docker system prune"))
}

async fn run_volume_prune() -> Result<PruneResult> {
    let output = Command::new("docker")
        .args(["volume", "prune", "--all", "--force"])
        .output()
        .await
        .context("failed to invoke `docker volume prune`")?;
    Ok(prune_result(&output, "docker volume prune"))
}

fn prune_result(output: &std::process::Output, operation: &str) -> PruneResult {
    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            operation,
            "prune failed"
        );
        return PruneResult {
            summary: CommandSummary::failed_one(),
            reclaimed: None,
        };
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reclaimed = parse_total_reclaimed(&stdout);
    info!(operation, reclaimed = %display_size(reclaimed), "prune completed");
    PruneResult {
        summary: CommandSummary::ok_one(),
        reclaimed,
    }
}

async fn run_reclaim_helper() -> Result<CommandSummary> {
    warn!("automatic Docker.raw reclamation stalled; running Docker Desktop reclaim helper");
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--privileged",
            "--pid=host",
            "docker/desktop-reclaim-space",
        ])
        .output()
        .await
        .context("failed to invoke Docker Desktop reclaim helper")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "Docker Desktop reclaim helper failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    info!("Docker Desktop reclaim helper completed");
    Ok(CommandSummary::ok_one())
}

fn system_prune_arguments(settings: &Settings) -> Vec<String> {
    let mut arguments = vec![
        "system".to_string(),
        "prune".to_string(),
        "--force".to_string(),
    ];
    if settings.all_images {
        arguments.push("--all".to_string());
    }
    arguments.extend(["--filter".to_string(), format!("until={}h", settings.hours)]);
    arguments
}

fn allocated_bytes(path: &Path) -> Result<Option<u64>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.blocks().saturating_mul(512))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("reading Docker disk image metadata: {}", path.display())),
    }
}

fn should_run_reclaim_helper(
    docker_reclaimed: Option<u64>,
    before: Option<u64>,
    after: Option<u64>,
) -> bool {
    docker_reclaimed.is_some_and(|bytes| bytes > 0)
        && before
            .zip(after)
            .is_some_and(|(before, after)| after >= before)
}

fn physical_bytes_freed(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    before
        .zip(after)
        .map(|(before, after)| before.saturating_sub(after))
}

fn sum_optional_bytes(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(bytes), None) | (None, Some(bytes)) => Some(bytes),
        (None, None) => None,
    }
}

fn display_size(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "unknown".to_string(), |bytes| format_size(bytes, BINARY))
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
        .map_or(s.len(), |(i, _)| i);
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
    // A parsed, non-negative byte count; truncating the fractional part to a
    // whole number of bytes is the intended behavior.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = (value * multiplier) as u64;
    Some(bytes)
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
        let expected = 9_209_000_000u64 + 8_069_000_000u64;
        assert_eq!(total, expected);
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

    fn settings() -> Settings {
        Settings {
            hours: 720,
            scope: "system".to_string(),
            all_images: true,
            volumes: false,
            reclaim_physical: true,
            disk_image: PathBuf::from("/tmp/test/Docker.raw"),
        }
    }

    #[test]
    fn system_prune_arguments_include_all_old_images() {
        assert_eq!(
            system_prune_arguments(&settings()),
            vec![
                "system",
                "prune",
                "--force",
                "--all",
                "--filter",
                "until=720h",
            ]
        );
    }

    #[test]
    fn system_prune_arguments_can_keep_tagged_images() {
        let mut settings = settings();
        settings.all_images = false;
        assert_eq!(
            system_prune_arguments(&settings),
            vec!["system", "prune", "--force", "--filter", "until=720h"]
        );
    }

    #[test]
    fn reclaim_helper_requires_reported_bytes_and_no_physical_drop() {
        let cases = [
            (Some(1_000), Some(10_000), Some(10_000)),
            (Some(1_000), Some(10_000), Some(11_000)),
            (Some(1_000), Some(10_000), Some(9_000)),
            (Some(0), Some(10_000), Some(10_000)),
            (None, Some(10_000), Some(10_000)),
            (Some(1_000), None, Some(10_000)),
            (Some(1_000), Some(10_000), None),
        ];
        assert_eq!(
            cases.map(|(reclaimed, before, after)| should_run_reclaim_helper(
                reclaimed, before, after
            )),
            [true, true, false, false, false, false, false,]
        );
    }

    #[test]
    fn physical_bytes_freed_never_reports_growth() {
        assert_eq!(
            [
                physical_bytes_freed(Some(10_000), Some(7_000)),
                physical_bytes_freed(Some(10_000), Some(11_000)),
                physical_bytes_freed(None, Some(7_000)),
            ],
            [Some(3_000), Some(0), None]
        );
    }

    #[test]
    fn optional_byte_totals_preserve_known_values() {
        assert_eq!(
            [
                sum_optional_bytes(Some(1_000), Some(2_000)),
                sum_optional_bytes(Some(1_000), None),
                sum_optional_bytes(None, Some(2_000)),
                sum_optional_bytes(None, None),
            ],
            [Some(3_000), Some(1_000), Some(2_000), None]
        );
    }
}
