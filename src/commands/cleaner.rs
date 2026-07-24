use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use tokio::process::Command;

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

    let mut verified: Vec<(PathBuf, String)> = Vec::with_capacity(labeled.len());
    let mut rejected = 0;
    for (project, label) in labeled {
        if git_allows_deletion(&project, config.junk).await? {
            verified.push((project, label));
        } else {
            rejected += 1;
        }
    }
    info!(
        verified = verified.len(),
        rejected, "after git-ignore verification"
    );

    let junk = config.junk;
    let summary = run_parallel(
        config.bar_label,
        verified,
        config.concurrency,
        config.dry_run,
        move |(project, label), progress| async move {
            delete_dir(&project.join(junk), label, &progress).await;
        },
    )
    .await;

    Ok(summary)
}

/// Require Git to identify the artifact directory as ignored, then independently
/// confirm that its contents contain no tracked files. A missing ignore rule, a
/// directory outside a worktree, or another `git check-ignore` rejection leaves
/// the directory untouched.
async fn git_allows_deletion(project: &Path, junk: &str) -> Result<bool> {
    let artifact = project.join(junk);
    let ignored = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["check-ignore", "--quiet", "--"])
        .arg(junk)
        .output()
        .await
        .with_context(|| format!("invoking `git check-ignore` for {}", artifact.display()))?;

    if !ignored.status.success() {
        let reason = if ignored.status.code() == Some(1) {
            "not ignored by Git".to_string()
        } else {
            let stderr = String::from_utf8_lossy(&ignored.stderr);
            format!("Git could not verify it: {}", stderr.trim())
        };
        warn!(path = %artifact.display(), reason, "keeping project artifact");
        return Ok(false);
    }

    let tracked = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["ls-files", "--cached", "--"])
        .arg(junk)
        .output()
        .await
        .with_context(|| format!("invoking `git ls-files` for {}", artifact.display()))?;
    ensure!(
        tracked.status.success(),
        "`git ls-files` failed for {}: {}",
        artifact.display(),
        String::from_utf8_lossy(&tracked.stderr).trim()
    );

    if !tracked.stdout.is_empty() {
        warn!(
            path = %artifact.display(),
            tracked = %String::from_utf8_lossy(&tracked.stdout).trim(),
            "keeping project artifact because it contains tracked files"
        );
        return Ok(false);
    }

    Ok(true)
}

fn basename(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
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
    use super::{Config, clean, project_label};
    use crate::commands::CommandSummary;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    const MARKER: &str = "project.marker";
    const ARTIFACT: &str = "generated-artifact";
    const CONTENTS: &[u8] = b"test artifact";

    fn create_project(ignored: bool) -> (TempDir, PathBuf) {
        let temporary = TempDir::new().expect("temporary directory is created");
        let project = temporary.path().join("alice-project");
        fs::create_dir_all(project.join(ARTIFACT)).expect("artifact directory is created");
        fs::write(project.join(MARKER), "").expect("marker is written");
        fs::write(project.join(ARTIFACT).join("output.bin"), CONTENTS)
            .expect("artifact contents are written");
        if ignored {
            fs::write(project.join(".gitignore"), format!("{ARTIFACT}/\n"))
                .expect("ignore file is written");
        }
        let status = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&project)
            .status()
            .expect("git is invoked");
        assert!(status.success(), "git repository is initialized");
        (temporary, project)
    }

    async fn clean_project(temporary: &TempDir, dry_run: bool) -> CommandSummary {
        clean(Config {
            bar_label: "test-project-artifacts",
            marker: MARKER,
            junk: ARTIFACT,
            roots: vec![temporary.path().to_path_buf()],
            depth: 2,
            days: 0,
            concurrency: 1,
            dry_run,
        })
        .await
        .expect("project cleanup succeeds")
    }

    #[tokio::test]
    async fn ignored_artifact_is_deleted() {
        let (temporary, project) = create_project(true);

        let summary = clean_project(&temporary, false).await;

        assert_eq!(
            summary,
            CommandSummary {
                bytes_freed: CONTENTS.len() as u64,
                items_ok: 1,
                items_failed: 0,
                items_skipped: 0,
            }
        );
        assert!(!project.join(ARTIFACT).exists());
    }

    #[tokio::test]
    async fn dry_run_keeps_ignored_artifact() {
        let (temporary, project) = create_project(true);

        let summary = clean_project(&temporary, true).await;

        assert_eq!(
            summary,
            CommandSummary {
                bytes_freed: CONTENTS.len() as u64,
                items_ok: 1,
                items_failed: 0,
                items_skipped: 0,
            }
        );
        assert!(project.join(ARTIFACT).is_dir());
    }

    #[tokio::test]
    async fn artifact_without_ignore_rule_is_kept() {
        let (temporary, project) = create_project(false);

        let summary = clean_project(&temporary, false).await;

        assert_eq!(summary, CommandSummary::default());
        assert!(project.join(ARTIFACT).is_dir());
    }

    #[tokio::test]
    async fn ignored_artifact_with_tracked_contents_is_kept() {
        let (temporary, project) = create_project(false);
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["add", "--"])
            .arg(ARTIFACT)
            .status()
            .expect("git is invoked");
        assert!(status.success(), "artifact contents are staged");
        fs::write(project.join(".gitignore"), format!("{ARTIFACT}/\n"))
            .expect("ignore file is written");

        let summary = clean_project(&temporary, false).await;

        assert_eq!(summary, CommandSummary::default());
        assert!(project.join(ARTIFACT).is_dir());
    }

    #[test]
    fn nested_project_under_root_yields_relative_path() {
        let roots = vec![PathBuf::from("/home/alice/projects")];
        let project = Path::new("/home/alice/projects/example/cli");
        assert_eq!(project_label(project, &roots), "example/cli");
    }

    #[test]
    fn deeper_monorepo_path_yields_full_chain() {
        let roots = vec![PathBuf::from("/home/alice/projects")];
        let project = Path::new("/home/alice/projects/example/packages/web");
        assert_eq!(project_label(project, &roots), "example/packages/web");
    }

    #[test]
    fn project_equal_to_root_yields_root_basename() {
        let roots = vec![PathBuf::from("/home/alice/projects")];
        let project = Path::new("/home/alice/projects");
        assert_eq!(project_label(project, &roots), "projects");
    }

    #[test]
    fn longest_matching_root_wins() {
        let roots = vec![
            PathBuf::from("/home/alice/projects"),
            PathBuf::from("/home/alice/projects/example"),
        ];
        let project = Path::new("/home/alice/projects/example/cli");
        assert_eq!(project_label(project, &roots), "cli");
    }

    #[test]
    fn no_matching_root_falls_back_to_basename() {
        let roots = vec![PathBuf::from("/home/alice/projects")];
        let project = Path::new("/var/tmp/example/web");
        assert_eq!(project_label(project, &roots), "web");
    }
}
