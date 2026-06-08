use std::fmt;
use std::fmt::Write as _;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LoggingConfig;
use crate::ui;

pub fn init(cfg: &LoggingConfig) {
    let _ = cfg; // reserved for future style knobs (indent width, glyphs)

    // Non-default `RUST_LOG` (e.g. `RUST_LOG=warn`) filters `info_span!` spans and
    // INFO `tree` events before they reach the layer, so command trees degrade to
    // plain, un-treed WARN/ERROR lines. Accepted limitation.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let tree = TreeLayer::new(ui::emit_line, ui::use_color());

    tracing_subscriber::registry()
        .with(filter)
        .with(tree)
        .init();
}

/// A buffered line waiting to learn whether it is a mid-span branch (`├─ `) or the
/// span's closing branch (`└─ `). Stored in the root span's extensions: `on_event`
/// flushes the previous buffer as `├─ ` and replaces it; `on_close` flushes the last
/// buffer as `└─ `. This is why the last event of a span automatically becomes the
/// closing branch, and the ~50 early-return cleaner sites need no changes.
struct PendingLine(String);

/// Custom tracing layer rendering one coherent tree per root span.
///
/// - `on_new_span` (root spans only): blank line, then `name k=v` header.
/// - `on_event`: format `LEVEL message, k=v` (message first), buffer it; flush the
///   prior buffered line prefixed `├─ `.
/// - `on_close` (root): flush the buffered line prefixed `└─ `.
/// - `target == "tree"` events render without a level label (item / summary lines).
/// - Out-of-span events print plain immediately, no connector.
/// - Nested spans are transparent: their events fold into the nearest root ancestor.
pub struct TreeLayer<E> {
    emit: E,
    color: bool,
}

impl<E> TreeLayer<E>
where
    E: Fn(&str),
{
    pub fn new(emit: E, color: bool) -> Self {
        Self { emit, color }
    }

    fn emit(&self, line: &str) {
        (self.emit)(line);
    }
}

impl<E> fmt::Debug for TreeLayer<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TreeLayer")
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

impl<S, E> Layer<S> for TreeLayer<E>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    E: Fn(&str) + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        // Only root spans get a header / blank line; nested spans are transparent.
        if span.parent().is_some() {
            return;
        }
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        self.emit("");
        self.emit(&format_span_header(span.name(), &visitor));
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let is_tree = event.metadata().target() == "tree";
        let line = format_event_line(*event.metadata().level(), &visitor, is_tree, self.color);

        // Fold into the nearest root ancestor's buffer when inside a span.
        let root = ctx
            .event_span(event)
            .map(|span| span.scope().from_root().next().unwrap_or(span));

        match root {
            Some(root) => {
                let mut ext = root.extensions_mut();
                if let Some(prev) = ext.remove::<PendingLine>() {
                    self.emit(&format!("├─ {}", prev.0));
                }
                ext.insert(PendingLine(line));
            }
            None => self.emit(&line),
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        // Only root spans flush a closing branch; nested spans hold no buffer.
        if span.parent().is_some() {
            return;
        }
        if let Some(pending) = span.extensions_mut().remove::<PendingLine>() {
            self.emit(&format!("└─ {}", pending.0));
        }
    }
}

/// Collects an event's / span's fields. The `message` field is special-cased; every
/// other field is appended in declaration order as `, k=v`.
#[derive(Default, Debug)]
struct FieldVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl FieldVisitor {
    fn record(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((name.to_string(), value));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field.name(), format!("{value:?}"));
    }
}

/// `name k=v k=v` — span name followed by its fields in declaration order.
fn format_span_header(name: &str, visitor: &FieldVisitor) -> String {
    let mut header = name.to_string();
    for (k, v) in &visitor.fields {
        let _ = write!(header, " {k}={v}");
    }
    header
}

/// `LEVEL message, k=v` — message first. `tree`-target events drop the level label.
/// WARN is colored yellow and ERROR red when `color` is set.
fn format_event_line(level: Level, visitor: &FieldVisitor, is_tree: bool, color: bool) -> String {
    let mut body = visitor.message.clone().unwrap_or_default();
    for (k, v) in &visitor.fields {
        if body.is_empty() {
            let _ = write!(body, "{k}={v}");
        } else {
            let _ = write!(body, ", {k}={v}");
        }
    }

    if is_tree {
        return body;
    }

    let label = match level {
        Level::WARN => ui::warn_label("WARN", color),
        Level::ERROR => ui::error_label("ERROR", color),
        Level::INFO => "INFO".to_string(),
        Level::DEBUG => "DEBUG".to_string(),
        Level::TRACE => "TRACE".to_string(),
    };

    if body.is_empty() {
        label
    } else {
        format!("{label} {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    /// Build a subscriber whose `TreeLayer` captures emitted lines into a shared
    /// buffer, run `body` against it, and return the captured lines.
    fn capture<F: FnOnce()>(color: bool, body: F) -> Vec<String> {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let layer = TreeLayer::new(
            move |line: &str| sink.lock().unwrap().push(line.to_string()),
            color,
        );
        let subscriber = Registry::default().with(layer);
        with_default(subscriber, body);
        Arc::into_inner(lines)
            .expect("layer dropped its Arc clone after with_default returned")
            .into_inner()
            .expect("capture mutex is never poisoned")
    }

    /// Shorthand for an owned empty `String` in `vec![...]` expectations.
    fn blank() -> String {
        String::new()
    }

    #[test]
    fn root_span_emits_blank_line_then_header_with_fields() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x", days = 3);
            let _g = span.enter();
        });
        assert_eq!(out, vec![blank(), "clean-x days=3".to_string()]);
    }

    #[test]
    fn single_event_becomes_closing_branch() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::warn!("careful now");
        });
        assert_eq!(
            out,
            vec![
                blank(),
                "clean-x".to_string(),
                "└─ WARN careful now".to_string(),
            ]
        );
    }

    #[test]
    fn two_events_become_mid_then_closing_branch() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::warn!("first");
            tracing::info!("second");
        });
        assert_eq!(
            out,
            vec![
                blank(),
                "clean-x".to_string(),
                "├─ WARN first".to_string(),
                "└─ INFO second".to_string(),
            ]
        );
    }

    #[test]
    fn tree_target_suppresses_level_label() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::info!(target: "tree", "✓ item  deleted");
        });
        assert_eq!(
            out,
            vec![
                blank(),
                "clean-x".to_string(),
                "└─ ✓ item  deleted".to_string(),
            ]
        );
    }

    #[test]
    fn warn_keeps_level_label() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::warn!("watch out");
        });
        assert_eq!(out.last().unwrap(), "└─ WARN watch out");
    }

    #[test]
    fn info_renders_message_first_then_fields() {
        let out = capture(false, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::info!(found = 6, "candidate cache subdirs");
        });
        assert_eq!(
            out.last().unwrap(),
            "└─ INFO candidate cache subdirs, found=6"
        );
    }

    #[test]
    fn out_of_span_event_prints_plain() {
        let out = capture(false, || {
            tracing::warn!("no span here");
        });
        assert_eq!(out, vec!["WARN no span here".to_string()]);
    }

    #[test]
    fn two_sequential_spans_are_blank_line_separated() {
        let out = capture(false, || {
            {
                let span = tracing::info_span!("first");
                let _g = span.enter();
                tracing::info!(target: "tree", "a");
            }
            {
                let span = tracing::info_span!("second");
                let _g = span.enter();
                tracing::info!(target: "tree", "b");
            }
        });
        assert_eq!(
            out,
            vec![
                blank(),
                "first".to_string(),
                "└─ a".to_string(),
                blank(),
                "second".to_string(),
                "└─ b".to_string(),
            ]
        );
    }

    #[test]
    fn zero_event_span_emits_header_only() {
        let out = capture(false, || {
            let span = tracing::info_span!("empty", k = "v");
            let _g = span.enter();
        });
        assert_eq!(out, vec![blank(), "empty k=v".to_string()]);
    }

    #[test]
    fn nested_span_events_fold_into_root() {
        let out = capture(false, || {
            let root = tracing::info_span!("root");
            let _g = root.enter();
            tracing::info!(target: "tree", "from root");
            {
                let child = tracing::info_span!("child");
                let _c = child.enter();
                tracing::info!(target: "tree", "from child");
            }
        });
        // No blank line / header for `child`; its event folds into `root`'s buffer.
        assert_eq!(
            out,
            vec![
                blank(),
                "root".to_string(),
                "├─ from root".to_string(),
                "└─ from child".to_string(),
            ]
        );
    }

    #[test]
    fn color_flag_wraps_warn_in_yellow_sgr() {
        let out = capture(true, || {
            let span = tracing::info_span!("clean-x");
            let _g = span.enter();
            tracing::warn!("careful");
        });
        assert_eq!(out.last().unwrap(), "└─ \x1b[33mWARN\x1b[0m careful");
    }
}
