//! Persistent progress bars + tree-formatted scrollback writer.
//!
//! There is one process-wide `MultiProgress`. Commands create one `CommandBar` (parent),
//! call `inc()` / `set_message()` as work progresses, and finish with `finish_ok()` /
//! `finish_err()` — the bar stays visible at its final position rather than being
//! removed. After completion, a command may print a static tree summary via `print_tree`.
//!
//! Tracing log events are written into scrollback through `MpWriter`, which routes every
//! line through `MultiProgress::println` so the live bar area is repainted cleanly.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io;
use std::sync::OnceLock;
use std::time::Duration;
use tracing_subscriber::fmt::MakeWriter;

static MP: OnceLock<MultiProgress> = OnceLock::new();

pub fn init() {
    let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8));
    let _ = MP.set(mp);
}

pub fn mp() -> &'static MultiProgress {
    MP.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8)))
}

/// Parent progress bar for a command. Holds total + current freed bytes etc.
pub struct CommandBar {
    bar: ProgressBar,
}

impl CommandBar {
    pub fn new(name: &str, total: u64) -> Self {
        let bar = mp().add(ProgressBar::new(total));
        bar.set_style(running_style());
        bar.set_prefix(name.to_string());
        bar.enable_steady_tick(Duration::from_millis(150));
        Self { bar }
    }

    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
    }

    pub fn set_message<S: Into<String>>(&self, msg: S) {
        self.bar.set_message(msg.into());
    }

    pub fn finish_ok<S: Into<String>>(self, msg: S) {
        self.bar.disable_steady_tick();
        self.bar.set_style(done_ok_style());
        self.bar.finish_with_message(msg.into());
    }

    pub fn finish_err<S: Into<String>>(self, msg: S) {
        self.bar.disable_steady_tick();
        self.bar.set_style(done_err_style());
        self.bar.finish_with_message(msg.into());
    }
}

fn running_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:.bold.cyan} {spinner} [{wide_bar:.green/dim}] {pos}/{len} {msg}",
    )
    .unwrap()
    .progress_chars("=> ")
    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn done_ok_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.green} ✓ {pos}/{len} {msg}").unwrap()
}

fn done_err_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.red} ✗ {pos}/{len} {msg}").unwrap()
}

/// Print a tree-shaped summary of `items` under `header`. Labels are padded so the
/// detail column lines up vertically.
pub fn print_tree(header: &str, items: &[TreeItem]) {
    let _ = mp().println("");
    let _ = mp().println(header);
    let max_label = items
        .iter()
        .map(|i| i.label.chars().count())
        .max()
        .unwrap_or(0);
    for (i, item) in items.iter().enumerate() {
        let branch = if i + 1 == items.len() {
            "└──"
        } else {
            "├──"
        };
        let icon = if item.ok { "✓" } else { "✗" };
        let label = pad_right(&item.label, max_label);
        let _ = mp().println(format!("  {branch} {icon} {label}  {}", item.detail));
    }
}

/// Format an elapsed duration in milliseconds as a human-readable string.
///
/// Rules:
/// - `<100ms` → `Xms` (e.g. `42ms`)
/// - `100ms-60s` → `X.Xs` (e.g. `9.6s`)
/// - `>=60s` → `XmYs` (e.g. `2m 15s`)
pub fn format_duration(ms: u64) -> String {
    if ms < 100 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let secs = ms as f64 / 1000.0;
        format!("{secs:.1}s")
    } else {
        let total_secs = ms / 1000;
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{minutes}m {seconds}s")
    }
}

fn pad_right(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        let mut out = String::from(s);
        out.extend(std::iter::repeat_n(' ', width - len));
        out
    }
}

#[derive(Debug)]
pub struct TreeItem {
    pub label: String,
    pub detail: String,
    pub ok: bool,
}

/// Writer that pipes tracing-formatted log lines through MultiProgress::println so they
/// land above the live bars cleanly.
pub struct MpWriter;

impl<'a> MakeWriter<'a> for MpWriter {
    type Writer = MpHandle;
    fn make_writer(&'a self) -> Self::Writer {
        MpHandle
    }
}

pub struct MpHandle;

impl io::Write for MpHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = std::str::from_utf8(buf).unwrap_or("");
        for line in s.split('\n') {
            if !line.is_empty() {
                let _ = mp().println(line);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_under_100ms() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(42), "42ms");
        assert_eq!(format_duration(99), "99ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(100), "0.1s");
        assert_eq!(format_duration(1_200), "1.2s");
        assert_eq!(format_duration(9_585), "9.6s");
        assert_eq!(format_duration(59_900), "59.9s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60_000), "1m 0s");
        assert_eq!(format_duration(135_000), "2m 15s");
        assert_eq!(format_duration(3_600_000), "60m 0s");
    }
}
