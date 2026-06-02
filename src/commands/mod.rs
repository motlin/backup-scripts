/// Aggregated outcome of a single command's `run`, used by `all` to print
/// a grand-total summary across every step.
///
/// `items_ok` / `items_failed` count sub-operations within the command
/// (per-repo, per-project, per-file). A command with no sub-structure
/// records its own success/failure as `items_ok = 1` or `items_failed = 1`.
/// Commands that were skipped (tool not installed, dir missing) record
/// `items_skipped = 1` via `CommandSummary::skipped_one()` so the skip is
/// reported distinctly rather than masquerading as a no-op success.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandSummary {
    pub bytes_freed: u64,
    pub items_ok: u64,
    pub items_failed: u64,
    pub items_skipped: u64,
}

impl CommandSummary {
    pub fn ok_one() -> Self {
        Self {
            items_ok: 1,
            ..Self::default()
        }
    }

    pub fn ok_one_with_bytes(bytes: u64) -> Self {
        Self {
            bytes_freed: bytes,
            items_ok: 1,
            ..Self::default()
        }
    }

    pub fn failed_one() -> Self {
        Self {
            items_failed: 1,
            ..Self::default()
        }
    }

    pub fn skipped_one() -> Self {
        Self {
            items_skipped: 1,
            ..Self::default()
        }
    }

    pub fn passed(&self) -> bool {
        self.items_failed == 0
    }

    /// True when the command did no work and only recorded skips (tool absent,
    /// dir missing). Distinct from a successful run that happened to free nothing.
    pub fn skipped(&self) -> bool {
        self.items_skipped > 0 && self.items_ok == 0 && self.items_failed == 0
    }

    pub fn items_total(&self) -> u64 {
        self.items_ok + self.items_failed
    }

    pub fn merge(&mut self, other: CommandSummary) {
        self.bytes_freed += other.bytes_freed;
        self.items_ok += other.items_ok;
        self.items_failed += other.items_failed;
        self.items_skipped += other.items_skipped;
    }
}

#[cfg(test)]
mod tests {
    use super::CommandSummary;

    #[test]
    fn skipped_one_is_only_skipped() {
        let s = CommandSummary::skipped_one();
        assert_eq!(s.items_skipped, 1);
        assert_eq!(s.items_ok, 0);
        assert_eq!(s.items_failed, 0);
        assert!(s.skipped());
        assert!(s.passed(), "a skip is not a failure");
    }

    #[test]
    fn ok_one_is_not_skipped() {
        let s = CommandSummary::ok_one();
        assert!(!s.skipped());
        assert!(s.passed());
    }

    #[test]
    fn failed_one_is_not_skipped_and_not_passed() {
        let s = CommandSummary::failed_one();
        assert!(!s.skipped());
        assert!(!s.passed());
    }

    #[test]
    fn merge_accumulates_skipped() {
        let mut total = CommandSummary::skipped_one();
        total.merge(CommandSummary::skipped_one());
        total.merge(CommandSummary::ok_one());
        assert_eq!(total.items_skipped, 2);
        assert_eq!(total.items_ok, 1);
        assert!(
            !total.skipped(),
            "a mix of ok and skipped is not a pure skip"
        );
    }

    #[test]
    fn items_total_counts_work_not_skips() {
        let s = CommandSummary::skipped_one();
        assert_eq!(s.items_total(), 0, "a skip did no work");
    }
}

pub mod all;
pub mod bz_cleanup;
pub mod clean_brew;
pub mod clean_cargo;
pub mod clean_chrome;
pub mod clean_cocoapods;
pub mod clean_cypress;
pub mod clean_docker;
pub mod clean_electron_caches;
pub mod clean_go_build;
pub mod clean_gradle;
pub mod clean_jetbrains;
pub mod clean_library_caches;
pub mod clean_m2;
pub mod clean_maven;
pub mod clean_mise;
pub mod clean_node;
pub mod clean_node_gyp;
pub mod clean_npm;
pub mod clean_pip;
pub mod clean_playwright;
pub mod clean_pnpm;
pub mod clean_tmp;
pub mod clean_trash;
pub mod clean_xcode;
pub mod clean_yarn;
pub mod cleaner;
pub mod git_maintenance;
