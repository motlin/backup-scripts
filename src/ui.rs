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
    let max_label = items.iter().map(|i| i.label.chars().count()).max().unwrap_or(0);
    for (i, item) in items.iter().enumerate() {
        let branch = if i + 1 == items.len() { "└──" } else { "├──" };
        let icon = if item.ok { "✓" } else { "✗" };
        let label = pad_right(&item.label, max_label);
        let _ = mp().println(format!("  {branch} {icon} {label}  {}", item.detail));
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
