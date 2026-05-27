use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_tree::HierarchicalLayer;

use crate::config::LoggingConfig;
use crate::ui;

pub const DEFAULT_INDENT_AMOUNT: usize = 2;

pub fn init(cfg: &LoggingConfig) {
    let _ = cfg; // reserved for future style knobs (indent width, glyphs)

    let tree = HierarchicalLayer::new(DEFAULT_INDENT_AMOUNT)
        .with_writer(ui::MpWriter)
        .with_targets(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_indent_lines(true)
        .with_bracketed_fields(false);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tree)
        .init();
}
