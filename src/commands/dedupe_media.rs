use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::{DedupeMediaConfig, expand_tilde, expand_tildes};
use crate::ui::format_duration;

const MEDIA_EXTENSION_FILTER: &str = concat!(
    "--ext-filter=onlyext:",
    "mp4,mov,m4v,webm,mkv,avi,mpeg,mpg,m2ts,flv,wmv,3gp,",
    "heic,heif,avif,jpg,jpeg,png,gif,webp,tiff,tif,rw2,dng,cr2,cr3,nef,arw,orf,",
    "m4a,mp3,aac,flac,wav,ogg,opus"
);

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Organized `iMessage` media directory. [config: `dedupe_media.pictures_dir`]
    #[arg(long)]
    pictures_dir: Option<PathBuf>,

    /// Complete `yt-dlp` directory to include. Repeat for multiple roots.
    /// [config: `dedupe_media.yt_dlp_dirs`]
    #[arg(long = "yt-dlp-dir")]
    yt_dlp_dirs: Vec<PathBuf>,
}

pub async fn run(args: Args, cfg: &DedupeMediaConfig, dry_run: bool) -> Result<CommandSummary> {
    async move {
        if !jdupes_available().await {
            warn!("jdupes not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let pictures_dir = resolve_pictures_dir(args.pictures_dir, cfg.pictures_dir.as_ref());
        let yt_dlp_dirs = resolve_yt_dlp_dirs(&args.yt_dlp_dirs, cfg.yt_dlp_dirs.as_deref());
        let mut summary = CommandSummary::default();
        let mut roots = Vec::new();
        if pictures_dir.is_dir() {
            roots.push(pictures_dir);
        } else {
            warn!(path = %pictures_dir.display(), "Pictures media directory missing — skipping");
            summary.items_skipped += 1;
        }

        for path in yt_dlp_dirs {
            if path.is_dir() {
                roots.push(path);
            } else {
                warn!(path = %path.display(), "yt-dlp directory missing — skipping");
                summary.items_skipped += 1;
            }
        }

        if roots.is_empty() {
            return Ok(summary);
        }
        let filters = [OsString::from(MEDIA_EXTENSION_FILTER)];
        summary.merge(run_jdupes(&roots, &filters, true, dry_run).await?);

        Ok(summary)
    }
    .instrument(info_span!("dedupe-media"))
    .await
}

fn resolve_pictures_dir(cli: Option<PathBuf>, configured: Option<&PathBuf>) -> PathBuf {
    cli.map(|path| expand_tilde(&path))
        .or_else(|| configured.map(|path| expand_tilde(path)))
        .unwrap_or_else(|| default_home_path("Pictures/iMessage"))
}

fn resolve_yt_dlp_dirs(cli: &[PathBuf], configured: Option<&[PathBuf]>) -> Vec<PathBuf> {
    if !cli.is_empty() {
        return expand_tildes(cli);
    }
    if let Some(paths) = configured
        && !paths.is_empty()
    {
        return expand_tildes(paths);
    }
    vec![
        default_home_path("Documents/yt-dlp"),
        default_home_path("Desktop/yt-dlp-from-Factorio2"),
    ]
}

fn default_home_path(relative: &str) -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())).join(relative)
}

async fn jdupes_available() -> bool {
    let output = Command::new("jdupes").arg("--version").output().await;
    matches!(output, Ok(result) if result.status.success())
}

async fn run_jdupes(
    roots: &[PathBuf],
    filters: &[OsString],
    preserve_root_order: bool,
    dry_run: bool,
) -> Result<CommandSummary> {
    let arguments = build_arguments(roots, filters, preserve_root_order, dry_run);
    let started = Instant::now();
    let output = Command::new("jdupes")
        .args(&arguments)
        .output()
        .await
        .context("failed to invoke jdupes")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        warn!(
            status = %output.status,
            stdout,
            stderr,
            "jdupes failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    let outcome = if stdout.is_empty() {
        "no duplicates found"
    } else {
        stdout.as_str()
    };
    info!(
        elapsed = %format_duration(started.elapsed().as_millis()),
        result = outcome,
        "jdupes completed"
    );
    Ok(CommandSummary::ok_one())
}

fn build_arguments(
    roots: &[PathBuf],
    filters: &[OsString],
    preserve_root_order: bool,
    dry_run: bool,
) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("--recurse"),
        OsString::from(if dry_run {
            "--summarize"
        } else {
            "--link-hard"
        }),
        OsString::from("--one-file-system"),
        OsString::from("--no-hidden"),
        OsString::from("--quiet"),
    ];
    if preserve_root_order {
        arguments.push(OsString::from("--param-order"));
    }
    arguments.extend_from_slice(filters);
    arguments.extend(roots.iter().map(|path| path.as_os_str().to_owned()));
    arguments
}

#[cfg(test)]
mod tests {
    use super::build_arguments;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn combined_dry_run_arguments_scan_all_media_roots_together() {
        let arguments = build_arguments(
            &[
                PathBuf::from("/example/Pictures/iMessage"),
                PathBuf::from("/example/Documents/yt-dlp"),
                PathBuf::from("/example/Desktop/yt-dlp"),
            ],
            &[OsString::from(super::MEDIA_EXTENSION_FILTER)],
            true,
            true,
        );

        assert_eq!(
            arguments,
            vec![
                OsString::from("--recurse"),
                OsString::from("--summarize"),
                OsString::from("--one-file-system"),
                OsString::from("--no-hidden"),
                OsString::from("--quiet"),
                OsString::from("--param-order"),
                OsString::from(super::MEDIA_EXTENSION_FILTER),
                OsString::from("/example/Pictures/iMessage"),
                OsString::from("/example/Documents/yt-dlp"),
                OsString::from("/example/Desktop/yt-dlp"),
            ]
        );
    }

    #[test]
    fn combined_live_arguments_hard_link_media_in_root_order() {
        let arguments = build_arguments(
            &[
                PathBuf::from("/example/Pictures/iMessage"),
                PathBuf::from("/example/Documents/yt-dlp"),
                PathBuf::from("/example/Desktop/yt-dlp"),
            ],
            &[OsString::from(super::MEDIA_EXTENSION_FILTER)],
            true,
            false,
        );

        assert_eq!(
            arguments,
            vec![
                OsString::from("--recurse"),
                OsString::from("--link-hard"),
                OsString::from("--one-file-system"),
                OsString::from("--no-hidden"),
                OsString::from("--quiet"),
                OsString::from("--param-order"),
                OsString::from(super::MEDIA_EXTENSION_FILTER),
                OsString::from("/example/Pictures/iMessage"),
                OsString::from("/example/Documents/yt-dlp"),
                OsString::from("/example/Desktop/yt-dlp"),
            ]
        );
    }
}
