use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LoggingConfig;
use crate::ui;

pub fn init(cfg: &LoggingConfig) {
    let _ = cfg; // reserved for future style knobs (indent width, glyphs)

    // Interim layer: `tracing-tree` was dropped (step 1 of the unified
    // tree-log-rendering plan); the custom `TreeLayer` replaces this in step 2.
    let tree = tracing_subscriber::fmt::layer()
        .with_writer(ui::MpWriter)
        .with_target(false)
        .with_level(true);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tree)
        .init();
}
