use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;
use tracing::{Instrument, info_span};

use crate::config::{CleanNodeConfig, resolve_roots};

use super::cleaner;

pub const DEFAULT_DEPTH: usize = 5;
pub const DEFAULT_DAYS: u32 = 14;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to search for Node projects. [config: clean_node.roots or roots, default: ~/projects]
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,

    /// Maximum directory depth when searching for package.json. [config: clean_node.depth, default: 5]
    #[arg(long)]
    pub depth: Option<usize>,

    /// Only clean node_modules/ dirs older than this many days. 0 = always clean. [config: clean_node.days, default: 14]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: clean_node.concurrency, default: 2]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(
    args: Args,
    cfg: &CleanNodeConfig,
    global_roots: &Option<Vec<PathBuf>>,
    dry_run: bool,
) -> Result<()> {
    let depth = args.depth.or(cfg.depth).unwrap_or(DEFAULT_DEPTH);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let roots = resolve_roots(args.roots, &cfg.roots, global_roots, "clean_node")?;

    cleaner::clean(
        "clean-node",
        "package.json",
        "node_modules",
        roots,
        depth,
        days,
        concurrency,
        dry_run,
    )
    .instrument(info_span!("clean-node"))
    .await
}
