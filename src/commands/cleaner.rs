use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use super::CommandSummary;
use super::executor::{delete_dir, run_parallel};
use crate::walk::{find_dirs_with_marker, older_than_days};

/// What to clean: where to look for projects, how to recognize them, and
/// which subdirectory to delete.
pub struct Config {
    pub bar_label: &'static str,
    pub marker: &'static str,
    pub junk: &'static str,
    pub roots: Vec<PathBuf>,
    pub depth: usize,
    pub days: u32,
    pub concurrency: usize,
    pub dry_run: bool,
}

/// Caller is responsible for wrapping this future in its own `info_span!` (e.g.
/// `info_span!("clean-maven")`) so the span name shows up in scrollback as the actual
/// command rather than a generic "cleaner".
pub async fn clean(config: Config) -> Result<CommandSummary> {
    let mut projects: Vec<PathBuf> = Vec::new();
    for root in &config.roots {
        if !root.exists() {
            warn!("root does not exist: {}", root.display());
            continue;
        }
        projects.extend(find_dirs_with_marker(root, config.marker, config.depth));
    }
    projects.sort();
    projects.dedup();

    let candidates: Vec<PathBuf> = projects
        .into_iter()
        .filter(|p| {
            let junk_path = p.join(config.junk);
            junk_path.is_dir() && older_than_days(&junk_path, config.days)
        })
        .collect();

    info!(found = candidates.len(), "candidates");

    let labeled: Vec<(PathBuf, String)> = candidates
        .into_iter()
        .map(|p| {
            let label = project_label(&p, &config.roots);
            (p, label)
        })
        .collect();

    let junk = config.junk;
    let summary = run_parallel(
        config.bar_label,
        labeled,
        config.concurrency,
        config.dry_run,
        move |(project, label), progress| async move {
            delete_dir(&project.join(junk), label, &progress).await;
        },
    )
    .await;

    Ok(summary)
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn project_label(project: &Path, roots: &[PathBuf]) -> String {
    // Longest matching root → shortest, most specific relative path.
    let best = roots
        .iter()
        .filter(|r| project.starts_with(r))
        .max_by_key(|r| r.components().count());
    if let Some(root) = best
        && let Ok(rel) = project.strip_prefix(root)
    {
        if !rel.as_os_str().is_empty() {
            return rel.display().to_string();
        }
        // project IS the root (marker at root top): use root basename.
        return basename(root);
    }
    basename(project) // defensive fallback
}

#[cfg(test)]
mod tests {
    use super::project_label;
    use std::path::{Path, PathBuf};

    #[test]
    fn nested_project_under_root_yields_relative_path() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects/motlin.com/cli");
        assert_eq!(project_label(project, &roots), "motlin.com/cli");
    }

    #[test]
    fn deeper_monorepo_path_yields_full_chain() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects/foo/packages/web");
        assert_eq!(project_label(project, &roots), "foo/packages/web");
    }

    #[test]
    fn project_equal_to_root_yields_root_basename() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/home/user/projects");
        assert_eq!(project_label(project, &roots), "projects");
    }

    #[test]
    fn longest_matching_root_wins() {
        let roots = vec![
            PathBuf::from("/home/user/projects"),
            PathBuf::from("/home/user/projects/motlin.com"),
        ];
        let project = Path::new("/home/user/projects/motlin.com/cli");
        assert_eq!(project_label(project, &roots), "cli");
    }

    #[test]
    fn no_matching_root_falls_back_to_basename() {
        let roots = vec![PathBuf::from("/home/user/projects")];
        let project = Path::new("/var/tmp/orphan/web");
        assert_eq!(project_label(project, &roots), "web");
    }
}
