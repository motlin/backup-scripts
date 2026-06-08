use anyhow::Result;
use clap::Args as ClapArgs;
use humansize::{BINARY, format_size};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::fs;
use tracing::{Instrument, info, info_span, warn};

use super::CommandSummary;
use crate::config::CleanTrashConfig;
use crate::ui::{self, CommandBar, ItemDetail, TreeItem, format_duration};
use crate::walk::dir_size;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {}

pub async fn run(_args: Args, cfg: &CleanTrashConfig, dry_run: bool) -> Result<CommandSummary> {
    let include_volumes = cfg.include_volumes.unwrap_or(true);
    let volumes_dir = cfg.volumes_dir.clone().unwrap_or_else(default_volumes_dir);
    let locations = trash_locations(include_volumes, &volumes_dir);

    let result = async move {
        if locations.is_empty() {
            info!("no trash directories found; skipping");
            return Ok::<_, anyhow::Error>(None);
        }

        // Flatten every location's entries into one work list of (path, label).
        // Each location keeps skipping `.DS_Store` and never yields the trash
        // directory itself — only its contents.
        let mut work: Vec<(PathBuf, String)> = Vec::new();
        for loc in &locations {
            match read_trash_entries(&loc.dir) {
                Ok(entries) => {
                    for path in entries {
                        let label = entry_label(&path, &loc.dir, &loc.prefix);
                        work.push((path, label));
                    }
                }
                Err(e) => warn!("cannot read {}: {e}", loc.dir.display()),
            }
        }
        info!(found = work.len(), "Trash entries");

        if work.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }

        let size_before = total_dir_size(&locations).await;
        let started = Instant::now();

        let bar = CommandBar::new("clean-trash", work.len() as u64);
        let total_bytes = AtomicU64::new(0);
        let total_count = AtomicU64::new(0);
        let items: Mutex<Vec<TreeItem>> = Mutex::new(Vec::new());

        // Process serially. Concurrent rm operations on the same parent directory
        // do not parallelize well, and the per-entry filesystem operations are quick.
        for (path, label) in work {
            clean_one(
                path,
                label,
                dry_run,
                &total_bytes,
                &total_count,
                &bar,
                &items,
            )
            .await;
        }

        let size_after = if dry_run {
            size_before
        } else {
            total_dir_size(&locations).await
        };
        let freed = size_before.saturating_sub(size_after);

        let count = total_count.load(Ordering::Relaxed);
        let verb = if dry_run { "would free" } else { "freed" };
        let measured = if dry_run {
            total_bytes.load(Ordering::Relaxed)
        } else {
            freed
        };
        let summary = format!(
            "{verb} {} across {count} items in {}ms",
            format_size(measured, BINARY),
            started.elapsed().as_millis()
        );

        bar.finish_ok(summary.clone());

        let items = items.into_inner().unwrap_or_default();
        Ok::<_, anyhow::Error>(Some((summary, items, measured)))
    }
    .instrument(info_span!("clean-trash"))
    .await?;

    if let Some((_summary, items, bytes)) = result {
        let items_ok = items.iter().filter(|i| i.ok).count() as u64;
        let items_failed = items.len() as u64 - items_ok;
        ui::emit_tree_items(&items);
        Ok(CommandSummary {
            bytes_freed: bytes,
            items_ok,
            items_failed,
            items_skipped: 0,
        })
    } else {
        Ok(CommandSummary::default())
    }
}

/// Read direct entries of the Trash directory, skipping `.DS_Store` which macOS
/// regenerates anyway. We do NOT delete the Trash directory itself — only its contents.
fn read_trash_entries(trash: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(trash)?.flatten() {
        let name = entry.file_name();
        if name == ".DS_Store" {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

/// Label for an entry: its path relative to the trash dir, prefixed with the
/// location's volume name when non-empty (e.g. `USB Drive/old-file.txt`) so
/// entries from different volumes are unambiguous. Home trash uses an empty
/// prefix, keeping labels identical to before.
fn entry_label(path: &Path, trash: &Path, prefix: &str) -> String {
    let rel = path
        .strip_prefix(trash)
        .unwrap_or(path)
        .display()
        .to_string();
    if prefix.is_empty() {
        rel
    } else {
        format!("{prefix}/{rel}")
    }
}

async fn clean_one(
    path: PathBuf,
    label: String,
    dry_run: bool,
    total_bytes: &AtomicU64,
    total_count: &AtomicU64,
    bar: &CommandBar,
    items: &Mutex<Vec<TreeItem>>,
) {
    let started = Instant::now();
    let is_dir = path.is_dir();
    let size = if is_dir {
        dir_size(&path).await.unwrap_or(0)
    } else {
        std::fs::symlink_metadata(&path).ok().map_or(0, |m| m.len())
    };

    let (ok, detail) = if dry_run {
        total_bytes.fetch_add(size, Ordering::Relaxed);
        total_count.fetch_add(1, Ordering::Relaxed);
        let detail = ItemDetail::dry_run("would delete", format_size(size, BINARY));
        (true, detail)
    } else {
        let res = if is_dir {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        match res {
            Ok(()) => {
                total_bytes.fetch_add(size, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let detail = ItemDetail::success(
                    "deleted",
                    format_size(size, BINARY),
                    format_duration(started.elapsed().as_millis()),
                );
                (true, detail)
            }
            Err(e) => {
                let detail = ItemDetail::failure(format!("{e}"));
                warn!("✗ {label}  {}", ui::format_detail(&detail));
                (false, detail)
            }
        }
    };

    bar.inc(1);
    let running_bytes = total_bytes.load(Ordering::Relaxed);
    let verb = if dry_run { "would free" } else { "freed" };
    bar.set_message(format!("{verb} {}", format_size(running_bytes, BINARY)));

    items.lock().unwrap().push(TreeItem { label, detail, ok });
}

fn default_trash_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".Trash")
}

/// Default directory under which macOS mounts volumes.
fn default_volumes_dir() -> PathBuf {
    PathBuf::from("/Volumes")
}

/// Build the ordered list of trash directories to sweep: the home trash first
/// (when it exists), followed by each discovered per-volume trash under
/// `volumes_dir` when `include_volumes` is set. Locations that don't exist are
/// omitted.
fn trash_locations(include_volumes: bool, volumes_dir: &Path) -> Vec<TrashLocation> {
    let mut locations = Vec::new();
    let home = default_trash_dir();
    if home.exists() {
        locations.push(TrashLocation {
            dir: home,
            prefix: String::new(),
        });
    }
    if include_volumes {
        locations.extend(discover_volume_trashes(volumes_dir, &current_uid()));
    }
    locations
}

/// Sum of `dir_size` across every location dir.
async fn total_dir_size(locations: &[TrashLocation]) -> u64 {
    let mut total = 0u64;
    for loc in locations {
        total += dir_size(&loc.dir).await.unwrap_or(0);
    }
    total
}

/// A single trash directory to empty, with a label prefix that disambiguates
/// entries from different volumes in the output tree ("" for the home trash).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrashLocation {
    dir: PathBuf,
    prefix: String,
}

/// The current user's numeric uid, obtained by shelling out to `id -u` (mirrors
/// the `du` shell-out in `walk::dir_size`, avoiding a `libc`/`nix` dependency).
/// Returns an empty string if the command fails.
fn current_uid() -> String {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The per-volume trash directory for `uid`: `<volume_root>/.Trashes/<uid>`.
fn volume_trash_path(volume_root: &Path, uid: &str) -> PathBuf {
    volume_root.join(".Trashes").join(uid)
}

/// Discover per-volume user trashes under `volumes_dir` (e.g. `/Volumes`).
///
/// For each subdirectory, include `<volume>/.Trashes/<uid>` only when it exists,
/// labeled with the volume's directory name. A missing or unreadable
/// `volumes_dir` yields an empty vec.
fn discover_volume_trashes(volumes_dir: &Path, uid: &str) -> Vec<TrashLocation> {
    let Ok(entries) = std::fs::read_dir(volumes_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let volume_root = entry.path();
        if !volume_root.is_dir() {
            continue;
        }
        let trash = volume_trash_path(&volume_root, uid);
        if trash.is_dir() {
            let prefix = volume_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(TrashLocation { dir: trash, prefix });
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, self-cleaning fixture root under the OS temp dir. Scoped by
    /// pid + `tag` so concurrent tests don't collide; removed and recreated fresh.
    fn fixture_root(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("backup-clean-trash-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn volume_trash_path_builds_trashes_uid() {
        assert_eq!(
            volume_trash_path(Path::new("/Volumes/USB Drive"), "501"),
            PathBuf::from("/Volumes/USB Drive/.Trashes/501")
        );
    }

    #[test]
    fn discover_volume_trashes_keeps_only_volumes_with_trash() {
        let uid = "501";
        let vols = fixture_root("discover");
        std::fs::create_dir_all(vols.join("DriveA").join(".Trashes").join(uid)).unwrap();
        std::fs::create_dir_all(vols.join("DriveB")).unwrap(); // mounted, but no trash
        std::fs::write(vols.join("not-a-volume"), b"x").unwrap(); // plain file, ignored

        let found = discover_volume_trashes(&vols, uid);

        assert_eq!(found.len(), 1, "only DriveA has a .Trashes/<uid>");
        assert_eq!(found[0].prefix, "DriveA");
        assert_eq!(found[0].dir, vols.join("DriveA").join(".Trashes").join(uid));

        std::fs::remove_dir_all(&vols).unwrap();
    }

    #[test]
    fn discover_volume_trashes_missing_dir_is_empty() {
        let missing =
            std::env::temp_dir().join(format!("backup-clean-trash-{}-absent", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(discover_volume_trashes(&missing, "501").is_empty());
    }

    #[test]
    fn entry_label_empty_prefix_is_relative() {
        let trash = Path::new("/Users/me/.Trash");
        let entry = trash.join("old-file.txt");
        assert_eq!(entry_label(&entry, trash, ""), "old-file.txt");
    }

    #[test]
    fn entry_label_volume_prefix_is_prepended() {
        let trash = Path::new("/Volumes/USB Drive/.Trashes/501");
        let entry = trash.join("old-file.txt");
        assert_eq!(
            entry_label(&entry, trash, "USB Drive"),
            "USB Drive/old-file.txt"
        );
    }

    #[test]
    fn trash_locations_gates_volumes_on_include_flag() {
        let uid = current_uid();
        let vols = fixture_root("gate-volumes");
        std::fs::create_dir_all(vols.join("DriveA").join(".Trashes").join(&uid)).unwrap();

        // The home trash is independent of the flag, so compare only the
        // volume-sourced (non-empty prefix) locations.
        let with_volumes: Vec<_> = trash_locations(true, &vols)
            .into_iter()
            .filter(|loc| !loc.prefix.is_empty())
            .collect();
        let without_volumes: Vec<_> = trash_locations(false, &vols)
            .into_iter()
            .filter(|loc| !loc.prefix.is_empty())
            .collect();

        assert_eq!(
            with_volumes.len(),
            1,
            "DriveA trash discovered when enabled"
        );
        assert_eq!(with_volumes[0].prefix, "DriveA");
        assert!(
            without_volumes.is_empty(),
            "no volume trashes discovered when include_volumes is false"
        );

        std::fs::remove_dir_all(&vols).unwrap();
    }

    #[test]
    fn read_trash_entries_skips_ds_store() {
        let dir = fixture_root("read-entries");
        std::fs::write(dir.join(".DS_Store"), b"x").unwrap();
        std::fs::write(dir.join("keep.txt"), b"y").unwrap();

        let entries = read_trash_entries(&dir).unwrap();

        assert_eq!(entries, vec![dir.join("keep.txt")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
