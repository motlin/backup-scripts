use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;
use tracing::{Instrument, info_span};

use crate::config::{CleanMavenConfig, resolve_roots};

use super::{CommandSummary, cleaner};

pub const DEFAULT_DEPTH: usize = 4;
pub const DEFAULT_DAYS: u32 = 7;
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Roots to search for Maven projects. [config: `clean_maven.roots` or roots, default: ~/projects]
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,

    /// Maximum directory depth when searching for poms. [config: `clean_maven.depth`, default: 4]
    #[arg(long)]
    pub depth: Option<usize>,

    /// Only clean Git-ignored target/ dirs older than this many days. 0 = always clean. [config: `clean_maven.days`, default: 7]
    #[arg(long)]
    pub days: Option<u32>,

    /// Maximum number of parallel deletions. [config: `clean_maven.concurrency`, default: 2]
    #[arg(long)]
    pub concurrency: Option<usize>,
}

pub async fn run(
    args: Args,
    cfg: &CleanMavenConfig,
    global_roots: Option<&Vec<PathBuf>>,
    dry_run: bool,
) -> Result<CommandSummary> {
    let depth = args.depth.or(cfg.depth).unwrap_or(DEFAULT_DEPTH);
    let days = args.days.or(cfg.days).unwrap_or(DEFAULT_DAYS);
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let roots = resolve_roots(&args.roots, cfg.roots.as_ref(), global_roots, "clean_maven")?;

    cleaner::clean(cleaner::Config {
        bar_label: "clean-maven",
        marker: "pom.xml",
        junk: "target",
        roots,
        depth,
        days,
        concurrency,
        dry_run,
    })
    .instrument(info_span!("clean-maven"))
    .await
}
