use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::{CleanXdgCacheConfig, expand_tilde};
use crate::ui::format_duration;
use crate::walk::{dir_size, older_than_days};

/// Only scrub node cache entries older than this many days. 0 = always clean.
pub const DEFAULT_NODE_DAYS: u32 = 14;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Base XDG cache directory. [config: `clean_xdg_cache.cache_dir`, default: $`XDG_CACHE_HOME` or ~/.cache]
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Only delete node cache entries older than this many days. 0 = always clean. [config: `clean_xdg_cache.node_days`, default: 14]
    #[arg(long)]
    pub node_days: Option<u32>,
}

/// Reclaim disk under the XDG cache (`~/.cache`) for tools that store regenerable
/// data there, each via its first-party subcommand where one exists.
///
/// Three independent operations, each safe by design and each skipped when its
/// tool/dir is absent:
///   * `uv cache prune` — removes unreachable cache entries. NEVER a raw
///     `rm ~/.cache/uv`: Astral warns the cache is never safe to modify directly
///     (live virtualenvs under `environments-v2` are hardlinked into it, and a
///     blanket delete during a running `uvx`/MCP can crash it — astral-sh/uv#11694).
///     `uv cache prune` has no dry-run flag, so a dry run only logs the intent.
///   * `pre-commit gc` — garbage-collects unused hook repos/environments. No
///     dry-run flag either, so a dry run only logs the intent.
///   * `node` cache scrub — `~/.cache/node` has no managing CLI, so we delete the
///     directory's CONTENTS directly (keeping the dir), honoring `--node-days`.
///
/// `github-copilot`/`copilot` are deliberately left alone: deleting them forces a
/// re-index/re-auth, which is not worth the reclaimed space without an explicit
/// opt-in.
pub async fn run(args: Args, cfg: &CleanXdgCacheConfig, dry_run: bool) -> Result<CommandSummary> {
    let cache_dir = args
        .cache_dir
        .or_else(|| cfg.cache_dir.clone())
        .map_or_else(default_cache_dir, |p| expand_tilde(&p));
    let node_days = args
        .node_days
        .or(cfg.node_days)
        .unwrap_or(DEFAULT_NODE_DAYS);

    async move {
        let mut summary = CommandSummary::default();
        summary.merge(run_uv_prune(dry_run).await?);
        summary.merge(run_precommit_gc(dry_run).await?);
        summary.merge(scrub_node_cache(&cache_dir.join("node"), node_days, dry_run).await);
        Ok(summary)
    }
    .instrument(info_span!("clean-xdg-cache"))
    .await
}

/// `uv cache prune` — removes unreachable cache entries. Skipped when uv is not on
/// PATH. There is no dry-run flag, so under `--dry-run` we only log the intent
/// rather than risk mutating a cache hardlinked into live virtualenvs.
async fn run_uv_prune(dry_run: bool) -> Result<CommandSummary> {
    if !tool_available("uv").await {
        warn!("uv not on PATH — skipping uv cache prune");
        return Ok(CommandSummary::skipped_one());
    }

    if dry_run {
        info!("dry run: would run `uv cache prune`");
        return Ok(CommandSummary::ok_one());
    }

    let started = Instant::now();
    let output = Command::new("uv")
        .arg("cache")
        .arg("prune")
        .output()
        .await
        .context("failed to invoke `uv cache prune`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "uv cache prune failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    info!(
        elapsed = %format_duration(started.elapsed().as_millis()),
        "`uv cache prune` removed unreachable cache entries"
    );
    Ok(CommandSummary::ok_one())
}

/// `pre-commit gc` — garbage-collects unused hook repos/environments. Skipped when
/// pre-commit is not on PATH. No dry-run flag, so under `--dry-run` we only log.
async fn run_precommit_gc(dry_run: bool) -> Result<CommandSummary> {
    if !tool_available("pre-commit").await {
        warn!("pre-commit not on PATH — skipping pre-commit gc");
        return Ok(CommandSummary::skipped_one());
    }

    if dry_run {
        info!("dry run: would run `pre-commit gc`");
        return Ok(CommandSummary::ok_one());
    }

    let started = Instant::now();
    let output = Command::new("pre-commit")
        .arg("gc")
        .output()
        .await
        .context("failed to invoke `pre-commit gc`")?;

    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "pre-commit gc failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    info!(
        elapsed = %format_duration(started.elapsed().as_millis()),
        "`pre-commit gc` removed unused hook environments"
    );
    Ok(CommandSummary::ok_one())
}

/// Scrub the CONTENTS of the node cache dir, keeping the dir itself. There is no
/// managing CLI for `~/.cache/node`, so a direct delete is the right tool; only
/// entries older than `days` are removed. A missing dir is a skip, not a failure.
async fn scrub_node_cache(node_dir: &Path, days: u32, dry_run: bool) -> CommandSummary {
    if !node_dir.is_dir() {
        info!(
            "node cache dir does not exist, skipping: {}",
            node_dir.display()
        );
        return CommandSummary::skipped_one();
    }

    let children = match read_children(node_dir) {
        Ok(c) => c,
        Err(e) => {
            warn!("cannot read {}: {e}", node_dir.display());
            return CommandSummary::failed_one();
        }
    };

    let mut freed: u64 = 0;
    let mut failed = false;
    for child in &children {
        if !older_than_days(child, days) {
            continue;
        }
        let size = dir_size(child).await.unwrap_or(0);
        if dry_run {
            freed += size;
            continue;
        }
        let res = if child.is_dir() && !child.is_symlink() {
            tokio::fs::remove_dir_all(child).await
        } else {
            tokio::fs::remove_file(child).await
        };
        match res {
            Ok(()) => freed += size,
            Err(e) => {
                warn!("failed to delete {}: {e}", child.display());
                failed = true;
            }
        }
    }

    let verb = if dry_run { "would free" } else { "freed" };
    info!("node cache: {verb} {}", format_size(freed, BINARY));

    if failed {
        CommandSummary {
            bytes_freed: freed,
            items_ok: 0,
            items_failed: 1,
            items_skipped: 0,
        }
    } else {
        CommandSummary::ok_one_with_bytes(freed)
    }
}

fn read_children(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        out.push(entry?.path());
    }
    Ok(out)
}

/// Returns true iff `tool` is installed and runnable (`<tool> --version` succeeds).
async fn tool_available(tool: &str) -> bool {
    let output = Command::new(tool).arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}

/// The XDG cache base: `$XDG_CACHE_HOME` when set and non-empty, else `~/.cache`.
fn default_cache_dir() -> PathBuf {
    match std::env::var("XDG_CACHE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".cache")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_node_dir_is_skip_not_failure() {
        let summary = scrub_node_cache(Path::new("/nonexistent/xdg/cache/node"), 0, true).await;
        assert!(summary.skipped());
        assert!(summary.passed());
        assert_eq!(summary.bytes_freed, 0);
    }

    #[tokio::test]
    async fn dry_run_keeps_node_cache_contents() {
        let tmp = std::env::temp_dir().join(format!("xdg-node-{}", std::process::id()));
        let node = tmp.join("node");
        std::fs::create_dir_all(node.join("entry")).unwrap();
        std::fs::write(node.join("entry/file.bin"), b"payload").unwrap();

        let summary = scrub_node_cache(&node, 0, true).await;

        assert!(summary.passed());
        assert!(
            node.join("entry/file.bin").exists(),
            "dry run must not delete cache contents"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn real_run_scrubs_node_cache_contents_but_keeps_dir() {
        let tmp = std::env::temp_dir().join(format!("xdg-node-real-{}", std::process::id()));
        let node = tmp.join("node");
        std::fs::create_dir_all(node.join("entry")).unwrap();
        std::fs::write(node.join("entry/file.bin"), b"payload").unwrap();

        let summary = scrub_node_cache(&node, 0, false).await;

        assert!(summary.passed());
        assert_eq!(summary.items_ok, 1);
        assert!(node.is_dir(), "the node dir itself must be kept");
        assert!(
            !node.join("entry").exists(),
            "real run must delete cache contents"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn node_days_filter_spares_recent_entries() {
        let tmp = std::env::temp_dir().join(format!("xdg-node-days-{}", std::process::id()));
        let node = tmp.join("node");
        std::fs::create_dir_all(&node).unwrap();
        std::fs::write(node.join("fresh.bin"), b"payload").unwrap();

        let summary = scrub_node_cache(&node, 9999, false).await;

        assert!(summary.passed());
        assert!(
            node.join("fresh.bin").exists(),
            "a freshly written entry must survive a large --node-days filter"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
