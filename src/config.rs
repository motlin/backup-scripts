use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub walk: WalkConfig,
    pub logging: LoggingConfig,
    pub git_maintenance: GitMaintenanceConfig,
    pub clean_maven: CleanMavenConfig,
    pub clean_node: CleanNodeConfig,
    pub clean_cargo: CleanCargoConfig,
    pub clean_m2: CleanM2Config,
    pub clean_gradle: CleanGradleConfig,
    pub clean_tmp: CleanTmpConfig,
    pub clean_xcode: CleanXcodeConfig,
    pub clean_docker: CleanDockerConfig,
    pub clean_brew: CleanBrewConfig,
    pub clean_npm: CleanNpmConfig,
    pub clean_trash: CleanTrashConfig,
    pub clean_yarn: CleanYarnConfig,
    pub clean_pnpm: CleanPnpmConfig,
    pub clean_pip: CleanPipConfig,
    pub clean_cocoapods: CleanCocoaPodsConfig,
    pub clean_go_build: CleanGoBuildConfig,
    pub clean_jetbrains: CleanJetBrainsConfig,
    pub clean_library_caches: CleanLibraryCachesConfig,
    pub clean_electron_caches: CleanElectronCachesConfig,
    pub clean_chrome: CleanChromeConfig,
    pub clean_playwright: CleanPlaywrightConfig,
    pub clean_cypress: CleanCypressConfig,
    pub clean_node_gyp: CleanNodeGypConfig,
    pub bz_cleanup: BzCleanupConfig,
    pub all: AllConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalkConfig {
    /// Directory names that are never descended into when scanning for project markers.
    pub prune_dirs: Option<Vec<String>>,
    /// Follow symlinks while walking. Defaults to false (avoids cycles and dependency mirrors).
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Maximum number of live spinners to display simultaneously.
    pub max_spinners: Option<u64>,
    /// Symbol drawn before each nested span (e.g. `"↳ "`).
    pub indent_symbol: Option<String>,
    /// Indentation string repeated once per span nesting level (e.g. `"  "`).
    pub indent_str: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitMaintenanceConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub depth: Option<usize>,
    pub concurrency: Option<usize>,
    /// Run maintenance on submodules too. Default true.
    pub submodules: Option<bool>,
    /// Per-repo overrides that skip specific maintenance steps (e.g. the
    /// expensive `repack`) for repos matched by path suffix.
    pub overrides: Option<Vec<GitMaintenanceOverride>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitMaintenanceOverride {
    /// Match any repo whose path ends with this value, compared component-wise
    /// (e.g. `projects/huge-monorepo` matches `/Users/me/projects/huge-monorepo`).
    pub repo: String,
    /// Maintenance step names to skip for matched repos. Valid names:
    /// `pack-refs`, `reflog-expire`, `rerere-gc`, `worktree-prune`, `repack`,
    /// `prune`, `commit-graph`.
    pub skip_steps: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanMavenConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub depth: Option<usize>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanNodeConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub depth: Option<usize>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanCargoConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub depth: Option<usize>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanM2Config {
    pub repo: Option<PathBuf>,
    pub days: Option<u32>,
    pub snapshots_only: Option<bool>,
    pub concurrency: Option<usize>,
    /// File extension used to identify version directories. Default: "pom".
    pub marker_extension: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanGradleConfig {
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanTmpConfig {
    pub roots: Option<Vec<PathBuf>>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanXcodeConfig {
    pub data_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanDockerConfig {
    /// Only prune objects older than this many hours. Default: 720 (30 days).
    pub hours: Option<u32>,
    /// Prune scope. Currently only "system" is supported.
    pub scope: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanBrewConfig {
    /// Value passed to `brew cleanup --prune=<value>`. Default: "all".
    pub prune: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanNpmConfig {
    /// Path to the npm cacache directory. Default: ~/.npm/_cacache.
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanTrashConfig {
    /// Also empty per-volume trashes under the volumes dir. Default: true.
    pub include_volumes: Option<bool>,
    /// Directory holding mounted volumes. Default: /Volumes. (Override for tests.)
    pub volumes_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanYarnConfig {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanPnpmConfig {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanPipConfig {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanCocoaPodsConfig {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanGoBuildConfig {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanJetBrainsConfig {
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanLibraryCachesConfig {
    pub paths: Option<Vec<PathBuf>>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanElectronCachesConfig {
    pub support_dir: Option<PathBuf>,
    pub apps: Option<Vec<String>>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanChromeConfig {
    pub chrome_dir: Option<PathBuf>,
    pub model_dirs: Option<Vec<String>>,
    pub profile_subdirs: Option<Vec<String>>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
    pub skip_running_check: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanPlaywrightConfig {
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanCypressConfig {
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanNodeGypConfig {
    pub cache_dir: Option<PathBuf>,
    pub days: Option<u32>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BzCleanupConfig {
    pub days: Option<u32>,
    pub dir: Option<PathBuf>,
    /// Glob pattern passed to `find -name`. Default: "bz_done_*.dat".
    pub pattern: Option<String>,
    /// Run the find/delete commands under sudo. Default: true.
    pub use_sudo: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllConfig {
    /// Ordered list of subcommands to invoke from `backup all`.
    pub steps: Option<Vec<String>>,
}

/// Load the first existing config file in XDG search order:
/// 1. `$XDG_CONFIG_HOME/backup/config.json5` (default `~/.config/backup/config.json5`)
/// 2. each `$XDG_CONFIG_DIRS` entry's `backup/config.json5`
///
/// A missing file is not an error — defaults are used.
pub fn load() -> Result<AppConfig> {
    for path in candidate_paths() {
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            let cfg: AppConfig = json5::from_str(&raw)
                .with_context(|| format!("parsing config file {}", path.display()))?;
            return Ok(cfg);
        }
    }
    Ok(AppConfig::default())
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let user_root = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())).join(".config")
        }
    };
    paths.push(user_root.join("backup/config.json5"));

    if let Ok(dirs) = std::env::var("XDG_CONFIG_DIRS") {
        for dir in dirs.split(':').filter(|s| !s.is_empty()) {
            paths.push(PathBuf::from(dir).join("backup/config.json5"));
        }
    }

    paths
}

/// Expand a leading `~` to `$HOME`. Other paths are returned unchanged.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if s == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    path.to_path_buf()
}

pub fn expand_tildes(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.iter().map(|p| expand_tilde(p)).collect()
}

/// Resolve walking roots from the CLI, the command-specific config, and the global config.
/// Errors if none of them provide a value — there is no hardcoded fallback.
pub fn resolve_roots(
    cli: Vec<PathBuf>,
    cmd_cfg: &Option<Vec<PathBuf>>,
    global_cfg: &Option<Vec<PathBuf>>,
    cmd_name: &str,
) -> Result<Vec<PathBuf>> {
    if !cli.is_empty() {
        return Ok(expand_tildes(cli));
    }
    if let Some(c) = cmd_cfg.as_ref()
        && !c.is_empty()
    {
        return Ok(expand_tildes(c.clone()));
    }
    if let Some(g) = global_cfg.as_ref()
        && !g.is_empty()
    {
        return Ok(expand_tildes(g.clone()));
    }
    bail!(
        "no roots configured for `{cmd_name}`: pass --root, or set `roots` (top-level) or `{cmd_name}.roots` in your config file"
    );
}
