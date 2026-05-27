use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use crate::config::{BzCleanupConfig, expand_tilde};
use crate::ui;

pub const DEFAULT_DIR: &str = "/Library/Backblaze.bzpkg/bzdata/bzbackup/bzdatacenter";
pub const DEFAULT_DAYS: u32 = 60;
pub const DEFAULT_PATTERN: &str = "bz_done_*.dat";
pub const DEFAULT_USE_SUDO: bool = true;
pub const HELPER_PATH: &str = "/usr/local/bin/backup-bz-cleanup-helper";

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Delete files older than this many days (mtime +N). [config: bz_cleanup.days, default: 60]
    #[arg(long)]
    days: Option<u32>,

    /// Override the bzdatacenter directory. [config: bz_cleanup.dir, default: /Library/Backblaze.bzpkg/...]
    /// Note: only honored on the direct-sudo path; the installed helper hardcodes its own path.
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Filename pattern passed to `find -name`. [config: bz_cleanup.pattern, default: bz_done_*.dat]
    #[arg(long)]
    pattern: Option<String>,

    /// Skip sudo. Only useful if the directory is already user-readable/writable.
    /// [config: bz_cleanup.use_sudo, default: true]
    #[arg(long)]
    no_sudo: bool,
}

pub async fn run(args: Args, cfg: &BzCleanupConfig, dry_run: bool) -> Result<()> {
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let dir = args
        .dir
        .or_else(|| cfg.dir.clone())
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    let pattern = args
        .pattern
        .or_else(|| cfg.pattern.clone())
        .unwrap_or_else(|| DEFAULT_PATTERN.to_string());
    let use_sudo = if args.no_sudo {
        false
    } else {
        cfg.use_sudo.unwrap_or(DEFAULT_USE_SUDO)
    };

    if !dir.exists() {
        bail!("directory does not exist: {}", dir.display());
    }

    let mode = ExecMode::resolve(use_sudo, &dir);

    // Direct sudo (no helper) needs an interactive password prompt. Prime it *before* any
    // span opens so the spinner doesn't overdraw the `Password:` prompt.
    if matches!(mode, ExecMode::DirectSudo) {
        prime_sudo_blocking()?;
    }

    async move {
        info!(?mode, "execution mode");
        let scan = scan(&dir, days, &pattern, mode).await?;

        if scan.count == 0 {
            info!("nothing to delete");
            return Ok(());
        }

        if dry_run {
            info!(
                "dry run: would delete {} files ({})",
                scan.count,
                format_size(scan.bytes, BINARY)
            );
            return Ok(());
        }

        delete(&dir, days, &pattern, mode, scan.count, scan.bytes).await?;
        Ok(())
    }
    .instrument(info_span!("bz-cleanup", days))
    .await
}

/// Which subprocess shape we use depends on what's installed and what the caller asked for.
#[derive(Clone, Copy, Debug)]
enum ExecMode {
    /// `sudo /usr/local/bin/backup-bz-cleanup-helper scan|delete <pattern> <days>`
    /// — NOPASSWD via /etc/sudoers.d/backup, works in cron.
    Helper,
    /// `sudo find <dir> …` — needs an interactive password prompt.
    DirectSudo,
    /// `find <dir> …` — no sudo at all. Only works if the user can read the directory.
    NoSudo,
}

impl ExecMode {
    fn resolve(use_sudo: bool, configured_dir: &Path) -> Self {
        if !use_sudo {
            return Self::NoSudo;
        }
        // The helper hardcodes the bzdatacenter path; only use it when the configured dir
        // matches, otherwise fall back to direct sudo find.
        let helper = Path::new(HELPER_PATH);
        if helper.exists() && configured_dir == Path::new(DEFAULT_DIR) {
            return Self::Helper;
        }
        Self::DirectSudo
    }
}

struct ScanResult {
    count: u64,
    bytes: u64,
}

async fn scan(dir: &Path, days: u32, pattern: &str, mode: ExecMode) -> Result<ScanResult> {
    async {
        let started = Instant::now();
        let output = match mode {
            ExecMode::Helper => Command::new("sudo")
                .arg("-n")
                .arg(HELPER_PATH)
                .arg("scan")
                .arg(pattern)
                .arg(days.to_string())
                .output()
                .await
                .context("failed to invoke bz-cleanup helper via sudo")?,
            ExecMode::DirectSudo => Command::new("sudo")
                .args(find_args(dir, days, pattern, false))
                .output()
                .await
                .context("failed to invoke sudo find")?,
            ExecMode::NoSudo => Command::new("find")
                .args(find_args(dir, days, pattern, false))
                .output()
                .await
                .context("failed to invoke find")?,
        };

        if !output.status.success() {
            bail!(
                "scan failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let mut count = 0u64;
        let mut bytes = 0u64;
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let size: u64 = line
                .parse()
                .with_context(|| format!("unexpected stat output: {line:?}"))?;
            count += 1;
            bytes += size;
        }

        info!(
            count,
            size = %format_size(bytes, BINARY),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "matched files"
        );

        Ok(ScanResult { count, bytes })
    }
    .instrument(info_span!("scan"))
    .await
}

async fn delete(
    dir: &Path,
    days: u32,
    pattern: &str,
    mode: ExecMode,
    count: u64,
    bytes: u64,
) -> Result<()> {
    async {
        let started = Instant::now();
        let status = match mode {
            ExecMode::Helper => Command::new("sudo")
                .arg("-n")
                .arg(HELPER_PATH)
                .arg("delete")
                .arg(pattern)
                .arg(days.to_string())
                .status()
                .await
                .context("failed to invoke bz-cleanup helper via sudo")?,
            ExecMode::DirectSudo => Command::new("sudo")
                .args(find_args(dir, days, pattern, true))
                .status()
                .await
                .context("failed to invoke sudo find -delete")?,
            ExecMode::NoSudo => Command::new("find")
                .args(find_args(dir, days, pattern, true))
                .status()
                .await
                .context("failed to invoke find -delete")?,
        };

        if !status.success() {
            bail!("find -delete failed: {status}");
        }

        info!(
            count,
            reclaimed = %format_size(bytes, BINARY),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "deleted files"
        );

        Ok(())
    }
    .instrument(info_span!("delete"))
    .await
}

fn find_args(dir: &Path, days: u32, pattern: &str, delete: bool) -> Vec<String> {
    let mut args = vec![
        dir.to_string_lossy().into_owned(),
        "-maxdepth".into(),
        "1".into(),
        "-name".into(),
        pattern.into(),
        "-mtime".into(),
        format!("+{days}"),
    ];
    if delete {
        args.push("-delete".into());
    } else {
        args.extend([
            "-exec".into(),
            "stat".into(),
            "-f".into(),
            "%z".into(),
            "{}".into(),
            "+".into(),
        ]);
    }
    args
}

fn prime_sudo_blocking() -> Result<()> {
    ui::mp().suspend(|| -> Result<()> {
        let status = std::process::Command::new("sudo")
            .arg("-v")
            .status()
            .context("failed to invoke sudo -v")?;
        if !status.success() {
            warn!("sudo -v failed; subsequent commands may prompt or fail");
        }
        Ok(())
    })
}
