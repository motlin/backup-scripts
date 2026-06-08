use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

/// Hardcoded list used when neither CLI nor config provides one.
const FALLBACK_PRUNE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    ".next",
    ".turbo",
    ".gradle",
    ".idea",
    ".vscode",
    ".venv",
    "venv",
    "__pycache__",
];

pub struct WalkOptions {
    pub prune_dirs: Vec<String>,
    pub follow_symlinks: bool,
}

impl WalkOptions {
    pub fn fallback() -> Self {
        Self {
            prune_dirs: FALLBACK_PRUNE_DIRS
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            follow_symlinks: false,
        }
    }
}

static WALK_OPTIONS: OnceLock<WalkOptions> = OnceLock::new();

pub fn init_walk_options(opts: WalkOptions) {
    let _ = WALK_OPTIONS.set(opts);
}

fn walk_opts() -> &'static WalkOptions {
    WALK_OPTIONS.get_or_init(WalkOptions::fallback)
}

/// Walk `root` and return every directory that contains `marker` (a file or subdir name).
///
/// Hidden directories (starting with `.`) are skipped, except when the marker itself is
/// hidden (e.g. `.git`). Directories named in the walk config's `prune_dirs` are skipped.
pub fn find_dirs_with_marker(root: &Path, marker: &str, max_depth: usize) -> Vec<PathBuf> {
    let opts = walk_opts();
    let prune: Vec<&str> = opts
        .prune_dirs
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let marker_is_hidden = marker.starts_with('.');

    let walker = WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(opts.follow_symlinks)
        .into_iter()
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if prune.contains(&name.as_ref()) {
                return false;
            }
            if name.starts_with('.') && !(marker_is_hidden && name == marker) {
                return false;
            }
            true
        });

    let mut results = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        if entry.path().join(marker).exists() {
            results.push(entry.path().to_path_buf());
        }
    }
    results
}

/// Total apparent size in bytes of `path`: the summed length of every regular
/// file at or under it (symlinks are counted as the link, not followed). Pure
/// Rust via `walkdir`, so it spawns no `du` subprocess — keeping every caller
/// (and its tests) hermetic and portable. The walk runs on a blocking thread so
/// it never stalls the async runtime. Unreadable entries are skipped rather than
/// aborting the sum.
pub async fn dir_size(path: &Path) -> Option<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|meta| meta.len())
            .sum()
    })
    .await
    .ok()
}

pub fn mtime_secs(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_secs())
}

pub fn older_than_days(path: &Path, days: u32) -> bool {
    if days == 0 {
        return true;
    }
    let Some(mtime) = mtime_secs(path) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let cutoff = now.saturating_sub(u64::from(days) * 86_400);
    mtime < cutoff
}

pub fn follow_symlinks() -> bool {
    walk_opts().follow_symlinks
}
