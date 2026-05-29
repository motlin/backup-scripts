/// Aggregated outcome of a single command's `run`, used by `all` to print
/// a grand-total summary across every step.
///
/// `items_ok` / `items_failed` count sub-operations within the command
/// (per-repo, per-project, per-file). A command with no sub-structure
/// records its own success/failure as `items_ok = 1` or `items_failed = 1`.
/// Commands that were skipped (tool not installed, dir missing) return
/// `CommandSummary::default()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandSummary {
    pub bytes_freed: u64,
    pub items_ok: u64,
    pub items_failed: u64,
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
            items_failed: 0,
        }
    }

    pub fn failed_one() -> Self {
        Self {
            items_failed: 1,
            ..Self::default()
        }
    }

    pub fn passed(&self) -> bool {
        self.items_failed == 0
    }

    pub fn items_total(&self) -> u64 {
        self.items_ok + self.items_failed
    }

    pub fn merge(&mut self, other: CommandSummary) {
        self.bytes_freed += other.bytes_freed;
        self.items_ok += other.items_ok;
        self.items_failed += other.items_failed;
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
pub mod clean_go_build;
pub mod clean_gradle;
pub mod clean_jetbrains;
pub mod clean_library_caches;
pub mod clean_m2;
pub mod clean_maven;
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
