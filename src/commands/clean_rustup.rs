use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;
use tracing::{Instrument, info, info_span, warn};
use walkdir::{DirEntry, WalkDir};

use super::CommandSummary;
use crate::config::{CleanRustupConfig, expand_tildes};
use crate::ui::format_duration;

pub const DEFAULT_DEPTH: usize = 5;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to scan for rust-toolchain and mise pins. [config:
    /// `clean_rustup.roots` or roots]
    #[arg(long = "root")]
    roots: Vec<PathBuf>,

    /// Maximum directory depth when scanning for toolchain pins.
    /// [config: `clean_rustup.depth`, default: 5]
    #[arg(long)]
    depth: Option<usize>,

    /// Actually uninstall the removable toolchains. Without this flag the command
    /// only previews what it would remove (never uninstalls). The active, default,
    /// pinned, and directory-overridden toolchains are always skipped regardless.
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
///     referenced by a directory override, rust-toolchain file, or mise config.
///   * Removal is opt-in: without `--remove` (or `clean_rustup.remove = true`)
///     and outside a real run, the command only previews.
///   * We never `rm` the toolchain directory directly — that would desync
///     rustup's `settings.toml`. Removal always goes through
///     `rustup toolchain uninstall <name>`.
pub async fn run(
    args: Args,
    cfg: &CleanRustupConfig,
    global_roots: Option<&Vec<PathBuf>>,
    dry_run: bool,
) -> Result<CommandSummary> {
    let remove = args.remove || cfg.remove.unwrap_or(false);
    let depth = args.depth.or(cfg.depth).unwrap_or(DEFAULT_DEPTH);
    let roots = if !args.roots.is_empty() {
        expand_tildes(&args.roots)
    } else if let Some(roots) = cfg.roots.as_ref() {
        expand_tildes(roots)
    } else {
        global_roots.map_or_else(Vec::new, |roots| expand_tildes(roots))
    };
    if remove && roots.is_empty() {
        anyhow::bail!("Rustup removal requires at least one configured project root");
    }

    async move {
        if !rustup_available().await {
            warn!("rustup not on PATH — skipping");
            return Ok(CommandSummary::skipped_one());
        }

        let started = Instant::now();

        let toolchains = list_toolchains().await?;
        let overridden = list_overridden_toolchains().await?;
        let referenced = discover_referenced_toolchains(&roots, depth)?;
        let removable = removable_toolchains(&toolchains, &overridden, &referenced);

        for line in toolchains.lines().filter(|line| !line.trim().is_empty()) {
            let name = line.split_whitespace().next().unwrap_or_default();
            if let Some(reason) = retention_reason(line, name, &overridden, &referenced) {
                info!("keeping toolchain {name}: {reason}");
            }
        }

        if removable.is_empty() {
            info!("no removable toolchains — every installation is in use");
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
fn removable_toolchains(
    list_output: &str,
    overridden: &HashSet<String>,
    referenced: &BTreeSet<String>,
) -> Vec<String> {
    list_output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let name = line.split_whitespace().next()?;
            if retention_reason(line, name, overridden, referenced).is_some() {
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

#[derive(Debug, Deserialize)]
struct ToolchainFile {
    toolchain: ToolchainSpec,
}

#[derive(Debug, Deserialize)]
struct ToolchainSpec {
    channel: String,
}

fn discover_referenced_toolchains(roots: &[PathBuf], depth: usize) -> Result<BTreeSet<String>> {
    let mut referenced = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            warn!("toolchain scan root does not exist: {}", root.display());
            continue;
        }
        for result in WalkDir::new(root)
            .max_depth(depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(include_toolchain_scan_entry)
        {
            let entry = result.with_context(|| {
                format!("scanning toolchain references under {}", root.display())
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            match path.file_name().and_then(|name| name.to_str()) {
                Some("rust-toolchain" | "rust-toolchain.toml") => {
                    referenced.insert(parse_rust_toolchain_file(path)?);
                }
                Some("config.toml")
                    if path
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == ".mise") =>
                {
                    if let Some(version) = parse_mise_rust_version(path)? {
                        referenced.insert(version);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(referenced)
}

fn include_toolchain_scan_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".venv" | "__pycache__" | "build" | "dist" | "node_modules" | "target")
    )
}

fn parse_rust_toolchain_file(path: &Path) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading toolchain pin {}", path.display()))?;
    if contents.trim_start().starts_with('[') {
        let parsed: ToolchainFile = toml::from_str(&contents)
            .with_context(|| format!("parsing toolchain pin {}", path.display()))?;
        Ok(parsed.toolchain.channel)
    } else {
        let channel = contents
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .with_context(|| format!("empty toolchain pin {}", path.display()))?;
        Ok(channel.to_string())
    }
}

fn parse_mise_rust_version(path: &Path) -> Result<Option<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading mise config {}", path.display()))?;
    let document: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("parsing mise config {}", path.display()))?;
    let Some(rust) = document.get("tools").and_then(|tools| tools.get("rust")) else {
        return Ok(None);
    };
    if let Some(version) = rust.as_str() {
        return Ok(Some(version.to_string()));
    }
    let version = rust
        .get("version")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("invalid Rust tool entry in {}", path.display()))?;
    Ok(Some(version.to_string()))
}

fn retention_reason(
    line: &str,
    name: &str,
    overridden: &HashSet<String>,
    referenced: &BTreeSet<String>,
) -> Option<&'static str> {
    let markers = line
        .split_once('(')
        .map_or("", |(_, rest)| rest.trim_end_matches(')'));
    if markers
        .split(',')
        .map(str::trim)
        .any(|marker| marker == "active" || marker == "default")
    {
        return Some("active or default");
    }
    if overridden.contains(name) {
        return Some("rustup directory override");
    }
    referenced
        .iter()
        .any(|reference| toolchain_matches_reference(name, reference))
        .then_some("project toolchain pin")
}

fn toolchain_matches_reference(installed: &str, reference: &str) -> bool {
    installed == reference
        || installed
            .strip_prefix(reference)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    fn pins(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn removable_excludes_active_and_default() {
        let out = "\
stable-aarch64-apple-darwin (active, default)
nightly-aarch64-apple-darwin
";
        assert_eq!(
            removable_toolchains(out, &HashSet::new(), &BTreeSet::new()),
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
            removable_toolchains(out, &HashSet::new(), &BTreeSet::new()),
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
            removable_toolchains(out, &overridden, &BTreeSet::new()),
            vec!["beta-aarch64-apple-darwin".to_string()]
        );
    }

    #[test]
    fn removable_empty_when_only_active_default() {
        let out = "stable-aarch64-apple-darwin (active, default)\n";
        assert_eq!(
            removable_toolchains(out, &HashSet::new(), &BTreeSet::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn removable_ignores_blank_lines() {
        let out = "\nstable-x (active, default)\n\nnightly-x\n\n";
        assert_eq!(
            removable_toolchains(out, &HashSet::new(), &BTreeSet::new()),
            vec!["nightly-x".to_string()]
        );
    }

    #[test]
    fn removable_excludes_project_pins_with_host_suffixes() {
        let out = "\
stable-aarch64-apple-darwin (default)
nightly-aarch64-apple-darwin
1.97.1-aarch64-apple-darwin
";
        assert_eq!(
            removable_toolchains(out, &HashSet::new(), &pins(&["nightly", "1.97.1"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn parses_toolchain_toml() {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let path = directory.path().join("rust-toolchain.toml");
        std::fs::write(&path, "[toolchain]\nchannel = \"nightly\"\n")
            .expect("toolchain file is written");

        assert_eq!(
            parse_rust_toolchain_file(&path).expect("toolchain file is parsed"),
            "nightly"
        );
    }

    #[test]
    fn discovers_legacy_toolchain_and_mise_pins() {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let first = directory.path().join("alice");
        let second = directory.path().join("bob/.mise");
        std::fs::create_dir_all(&first).expect("first project is created");
        std::fs::create_dir_all(&second).expect("second project is created");
        std::fs::write(first.join("rust-toolchain"), "nightly\n")
            .expect("legacy toolchain file is written");
        std::fs::write(
            second.join("config.toml"),
            "[tools]\nrust = { version = \"1.97.1\", components = \"clippy\" }\n",
        )
        .expect("mise config is written");

        assert_eq!(
            discover_referenced_toolchains(&[directory.path().to_path_buf()], 5)
                .expect("toolchains are discovered"),
            pins(&["1.97.1", "nightly"])
        );
    }

    #[test]
    fn parse_override_no_overrides() {
        assert_eq!(
            parse_override_toolchains("no overrides\n"),
            HashSet::<String>::new()
        );
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
