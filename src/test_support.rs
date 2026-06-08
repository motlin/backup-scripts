//! Test-only shared helpers.

use std::sync::Once;

use tracing_subscriber::util::SubscriberInitExt;

static INSTALL: Once = Once::new();

/// Install a process-wide, always-interested default subscriber for the test
/// binary.
///
/// `tracing` caches each callsite's interest globally and recomputes it against
/// whatever dispatcher is the current default whenever a new callsite is first
/// hit. Many tests across the command modules emit the shared `info!(target:
/// "tree", ...)` callsites in `executor::finish` / `ui::emit_tree_items` with no
/// subscriber installed, which caches those callsites as `never`. The
/// `TreeLayer` capture tests then overlay their own subscriber via `with_default`
/// but the events are dropped before they reach it, because interest is still
/// cached as `never` — a flaky, scheduling-dependent failure.
///
/// A bare `Registry` as the global default keeps the fallback interested in
/// every callsite, so interest is never cached as `never` and capture tests
/// reliably receive their events. Capture tests still overlay their own
/// subscriber per-thread with `with_default`; this only fixes the global
/// fallback. Idempotent and safe to call from every test.
pub fn install_global_tracing_interest() {
    INSTALL.call_once(|| {
        let _ = tracing_subscriber::registry().try_init();
    });
}
