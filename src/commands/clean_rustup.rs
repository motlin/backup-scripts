use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::collections::HashSet;
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanRustupConfig;
use crate::ui::format_duration;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Actually uninstall the removable toolchains. Without this flag the command
    /// only previews what it would remove (never uninstalls). The active, default,
    /// and any directory-overridden toolchains are always skipped regardless.
    /// [config: `clean_rustup.remove`, default: false]
    #[arg(long)]
    remove: bool,
}

/// Uninstall non-default rustup toolchains via `rustup toolchain uninstall`.
///
/// rustup has no built-in garbage collection (rust-lang/rustup#4548), so stray
/// toolchains (a `nightly` left over from a one-off build, say) accumulate and
/// can run to gigabytes. This command fills that gap.
///
/// Safety:
///   * We only ever remove toolchains that are NEITHER active/default NOR
///     referenced by a directory override (`rustup override list`).
///   * Removal is opt-in: without `--remove` (or `clean_rustup.remove = true`)
///     and outside a real run, the command only previews.
///   * We never `rm` the toolchain directory directly — that would desync
///     rustup's `settings.toml`. Removal always goes through
///     `rustup toolchain uninstall <name>`.
pub async fn run(args: Args, cfg: &CleanRustupConfig, dry_run: bool) -> Result<CommandSummary> {
    let remove = args.remove || cfg.remove.unwrap_or(false);

    async move {
        if !rustup_available().await {
            warn!("rustup not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let started = Instant::now();

        let toolchains = list_toolchains().await?;
        let overridden = list_overridden_toolchains().await?;
        let removable = removable_toolchains(&toolchains, &overridden);

        if removable.is_empty() {
            info!("no removable toolchains — only active/default/overridden present");
            return Ok(CommandSummary::ok_one());
        }

        // Preview when not actually removing: either a global --dry-run, or the
        // user has not opted into removal.
        if dry_run || !remove {
            for name in &removable {
                info!("would uninstall toolchain {name}");
            }
            let why = if dry_run { "dry run" } else { "no --remove" };
            info!(
                elapsed = %format_duration(started.elapsed().as_millis()),
                "{why}: would uninstall {} toolchain(s)",
                removable.len(),
            );
            // A preview did no destructive work; report it as a single ok.
            return Ok(CommandSummary::ok_one());
        }

        let mut summary = CommandSummary::default();
        for name in &removable {
            summary.merge(uninstall_toolchain(name).await?);
        }

        info!(
            elapsed = %format_duration(started.elapsed().as_millis()),
            "uninstalled {} of {} toolchain(s)",
            summary.items_ok,
            removable.len(),
        );
        Ok(summary)
    }
    .instrument(info_span!("clean-rustup"))
    .await
}

/// Returns true iff the `rustup` CLI is installed and runnable.
async fn rustup_available() -> bool {
    let output = Command::new("rustup").arg("--version").output().await;
    matches!(output, Ok(out) if out.status.success())
}

/// Run `rustup toolchain list` and return its stdout.
async fn list_toolchains() -> Result<String> {
    let output = Command::new("rustup")
        .arg("toolchain")
        .arg("list")
        .output()
        .await
        .context("failed to invoke `rustup toolchain list`")?;
    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "rustup toolchain list failed"
        );
        anyhow::bail!("`rustup toolchain list` exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `rustup override list` and return the set of toolchain names referenced by
/// directory overrides. An empty set when there are no overrides.
async fn list_overridden_toolchains() -> Result<HashSet<String>> {
    let output = Command::new("rustup")
        .arg("override")
        .arg("list")
        .output()
        .await
        .context("failed to invoke `rustup override list`")?;
    if !output.status.success() {
        warn!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "rustup override list failed"
        );
        anyhow::bail!("`rustup override list` exited with {}", output.status);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_override_toolchains(&stdout))
}

/// Uninstall a single toolchain via `rustup toolchain uninstall <name>`.
async fn uninstall_toolchain(name: &str) -> Result<CommandSummary> {
    let output = Command::new("rustup")
        .arg("toolchain")
        .arg("uninstall")
        .arg(name)
        .output()
        .await
        .with_context(|| format!("failed to invoke `rustup toolchain uninstall {name}`"))?;

    if !output.status.success() {
        warn!(
            toolchain = %name,
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "rustup toolchain uninstall failed"
        );
        return Ok(CommandSummary::failed_one());
    }

    info!("uninstalled toolchain {name}");
    Ok(CommandSummary::ok_one())
}

/// Parse the name of each toolchain from `rustup toolchain list` output, keeping
/// only those safe to remove: not `(active, ...)`, not `(default ...)`, and not
/// referenced by a directory override.
///
/// `rustup toolchain list` lines look like:
///   `stable-aarch64-apple-darwin (active, default)`
///   `nightly-aarch64-apple-darwin`
///   `1.75.0-aarch64-apple-darwin (active)`
/// The marker is the parenthesized suffix; the toolchain name is the first
/// whitespace-delimited token.
fn removable_toolchains(list_output: &str, overridden: &HashSet<String>) -> Vec<String> {
    list_output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let name = line.split_whitespace().next()?;
            // The parenthesized markers tell us this toolchain is in use.
            let markers = line
                .split_once('(')
                .map_or("", |(_, rest)| rest.trim_end_matches(')'));
            let in_use = markers
                .split(',')
                .map(str::trim)
                .any(|m| m == "active" || m == "default");
            if in_use || overridden.contains(name) {
                return None;
            }
            Some(name.to_string())
        })
        .collect()
}

/// Parse toolchain names from `rustup override list` output. Each override line is
/// `<path>\t<toolchain>` (tab-separated). When there are no overrides rustup prints
/// `no overrides`, which yields an empty set.
fn parse_override_toolchains(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line == "no overrides" {
                return None;
            }
            // The toolchain name is the last whitespace-delimited token; the path
            // (which may contain spaces) precedes it, separated by a tab.
            let name = line.rsplit('\t').next()?.trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn removable_excludes_active_and_default() {
        let out = "\
stable-aarch64-apple-darwin (active, default)
nightly-aarch64-apple-darwin
";
        assert_eq!(
            removable_toolchains(out, &HashSet::new()),
            vec!["nightly-aarch64-apple-darwin".to_string()]
        );
    }

    #[test]
    fn removable_excludes_active_only() {
        let out = "\
stable-aarch64-apple-darwin (default)
1.75.0-aarch64-apple-darwin (active)
nightly-aarch64-apple-darwin
";
        assert_eq!(
            removable_toolchains(out, &HashSet::new()),
            vec!["nightly-aarch64-apple-darwin".to_string()]
        );
    }

    #[test]
    fn removable_excludes_overridden() {
        let out = "\
stable-aarch64-apple-darwin (active, default)
nightly-aarch64-apple-darwin
beta-aarch64-apple-darwin
";
        let overridden = set(&["nightly-aarch64-apple-darwin"]);
        assert_eq!(
            removable_toolchains(out, &overridden),
            vec!["beta-aarch64-apple-darwin".to_string()]
        );
    }

    #[test]
    fn removable_empty_when_only_active_default() {
        let out = "stable-aarch64-apple-darwin (active, default)\n";
        assert!(removable_toolchains(out, &HashSet::new()).is_empty());
    }

    #[test]
    fn removable_ignores_blank_lines() {
        let out = "\nstable-x (active, default)\n\nnightly-x\n\n";
        assert_eq!(
            removable_toolchains(out, &HashSet::new()),
            vec!["nightly-x".to_string()]
        );
    }

    #[test]
    fn parse_override_no_overrides() {
        assert!(parse_override_toolchains("no overrides\n").is_empty());
    }

    #[test]
    fn parse_override_single() {
        let out = "/Users/me/project\tnightly-aarch64-apple-darwin\n";
        assert_eq!(
            parse_override_toolchains(out),
            set(&["nightly-aarch64-apple-darwin"])
        );
    }

    #[test]
    fn parse_override_path_with_spaces() {
        let out = "/Users/me/my project\tbeta-aarch64-apple-darwin\n";
        assert_eq!(
            parse_override_toolchains(out),
            set(&["beta-aarch64-apple-darwin"])
        );
    }

    #[test]
    fn parse_override_multiple() {
        let out = "\
/a/b\tnightly-aarch64-apple-darwin
/c/d\t1.75.0-aarch64-apple-darwin
";
        assert_eq!(
            parse_override_toolchains(out),
            set(&[
                "nightly-aarch64-apple-darwin",
                "1.75.0-aarch64-apple-darwin",
            ])
        );
    }
}
