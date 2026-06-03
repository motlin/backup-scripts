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
use std::io::{self, IsTerminal};
use std::sync::OnceLock;
use std::time::Duration;
use tracing_subscriber::fmt::MakeWriter;

static MP: OnceLock<MultiProgress> = OnceLock::new();
static INTERACTIVE: OnceLock<bool> = OnceLock::new();
static USE_COLOR: OnceLock<bool> = OnceLock::new();

// Dependency-free SGR (Select Graphic Rendition) escapes, matching the repo's
// existing hand-rolled ANSI in indicatif templates — no new crate.
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m"; // B, ms
const BLUE: &str = "\x1b[34m"; // KiB
const GREEN: &str = "\x1b[32m"; // MiB, s
const YELLOW: &str = "\x1b[33m"; // GiB, m
const RED: &str = "\x1b[31m"; // TiB+, h

pub fn init() {
    let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8));
    let _ = MP.set(mp);
}

pub fn mp() -> &'static MultiProgress {
    MP.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8)))
}

/// Whether stderr is a terminal. When it isn't (cron, pipes, log files),
/// indicatif draws to a hidden target and `MultiProgress::println` silently
/// drops every line — so callers must fall back to plain stderr instead.
pub fn interactive() -> bool {
    *INTERACTIVE.get_or_init(|| io::stderr().is_terminal())
}

/// Whether to emit ANSI color escapes. Gated on an interactive TTY and the
/// absence of the `NO_COLOR` environment variable. Memoized like `interactive`.
fn use_color() -> bool {
    *USE_COLOR.get_or_init(|| interactive() && std::env::var_os("NO_COLOR").is_none())
}

/// Emit one scrollback line. In a terminal it goes through `MultiProgress` so the
/// live progress bars repaint cleanly; otherwise it goes straight to stderr so
/// non-interactive runs aren't silent.
pub fn emit_line(line: &str) {
    if interactive() {
        let _ = mp().println(line);
    } else {
        eprintln!("{line}");
    }
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
    emit_line("");
    emit_line(header);
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
        let detail = format_detail(&item.detail);
        emit_line(&format!("  {branch} {icon} {label}  {detail}"));
    }
}

/// Widths chosen so the widest expected value fits without wrapping:
/// `"would delete"` (12), `"164.54 MiB"` (10), `"2m 15s"` (6 — rounded up to 7).
pub const VERB_COL_WIDTH: usize = 12;
pub const SIZE_COL_WIDTH: usize = 10;
pub const DURATION_COL_WIDTH: usize = 7;

#[derive(Debug, Clone)]
pub struct ItemDetail {
    pub verb: String,
    pub size: Option<String>,
    pub duration: Option<String>,
    pub suffix: Option<String>,
}

impl ItemDetail {
    pub fn success(
        verb: impl Into<String>,
        size: impl Into<String>,
        duration: impl Into<String>,
    ) -> Self {
        Self {
            verb: verb.into(),
            size: Some(size.into()),
            duration: Some(duration.into()),
            suffix: None,
        }
    }

    pub fn dry_run(verb: impl Into<String>, size: impl Into<String>) -> Self {
        Self {
            verb: verb.into(),
            size: Some(size.into()),
            duration: None,
            suffix: None,
        }
    }

    pub fn failure(suffix: impl Into<String>) -> Self {
        Self {
            verb: "failed".to_string(),
            size: None,
            duration: None,
            suffix: Some(suffix.into()),
        }
    }

    pub fn with_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = Some(suffix.into());
        self
    }
}

/// Render a detail in aligned columns: `<verb> <size> in <duration><suffix>`.
/// Missing size or duration are rendered as blank padding so subsequent columns
/// still line up. Failure (no size, no duration) is rendered as `<verb> <suffix>`.
pub fn format_detail(d: &ItemDetail) -> String {
    format_detail_colored(d, use_color())
}

/// Inner implementation of [`format_detail`] with explicit color gating, so tests
/// can render deterministically regardless of whether the runner has a TTY.
fn format_detail_colored(d: &ItemDetail, color: bool) -> String {
    let verb = pad_right(&d.verb, VERB_COL_WIDTH);

    if d.size.is_none() && d.duration.is_none() {
        return match &d.suffix {
            Some(s) => format!("{verb} {s}"),
            None => verb,
        };
    }

    let size_plain = d.size.as_deref().unwrap_or("");
    let size = pad_right_colored(
        size_plain,
        &colorize_size(size_plain, color),
        SIZE_COL_WIDTH,
    );

    let tail = match &d.duration {
        Some(dur) => format!(
            " in {}",
            pad_right_colored(dur, &colorize_duration(dur, color), DURATION_COL_WIDTH)
        ),
        None => " ".repeat(DURATION_COL_WIDTH + 4),
    };

    let suffix = d.suffix.as_deref().unwrap_or("");
    let s = format!("{verb} {size}{tail}{suffix}");
    s.trim_end().to_string()
}

/// Format an elapsed duration in milliseconds as a human-readable string.
///
/// Rules:
/// - `<100ms` → `Xms` (e.g. `42ms`)
/// - `100ms-60s` → `X.Xs` (e.g. `9.6s`)
/// - `60s-60m` → `XmYs` (e.g. `2m 15s`)
/// - `>=60m` → `XhYm` (e.g. `1h 5m`)
pub fn format_duration(ms: u64) -> String {
    if ms < 100 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let secs = ms as f64 / 1000.0;
        format!("{secs:.1}s")
    } else {
        let total_secs = ms / 1000;
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        let seconds = total_secs % 60;
        if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m {seconds}s")
        }
    }
}

/// Wrap a `humansize::format_size(_, BINARY)` string (e.g. `4 KiB`, `512 B`) in an
/// ANSI color chosen by the magnitude of its unit: `B` dim, `KiB` blue, `MiB` green,
/// `GiB` yellow, `TiB`/`PiB`/`EiB` red. Returns `plain` unchanged when `!enabled`,
/// when `plain` is empty, or when the trailing unit is unrecognized.
fn colorize_size(plain: &str, enabled: bool) -> String {
    if !enabled || plain.is_empty() {
        return plain.to_string();
    }
    let unit = plain.split_whitespace().next_back().unwrap_or("");
    let color = match unit {
        "B" => DIM,
        "KiB" => BLUE,
        "MiB" => GREEN,
        "GiB" => YELLOW,
        "TiB" | "PiB" | "EiB" => RED,
        _ => return plain.to_string(),
    };
    format!("{color}{plain}{RESET}")
}

/// Color each whitespace-separated segment of a `format_duration` string (e.g.
/// `42ms`, `9.6s`, `2m 15s`, `1h 5m`) by the magnitude of its trailing unit:
/// `ms` dim, `s` green, `m` yellow, `h` red (checking `ms` before `s`). Returns
/// `plain` unchanged when `!enabled`; a segment with an unrecognized unit is
/// emitted without color.
fn colorize_duration(plain: &str, enabled: bool) -> String {
    if !enabled {
        return plain.to_string();
    }
    plain
        .split_whitespace()
        .map(|token| {
            let color = if token.ends_with("ms") {
                DIM
            } else if token.ends_with('s') {
                GREEN
            } else if token.ends_with('m') {
                YELLOW
            } else if token.ends_with('h') {
                RED
            } else {
                return token.to_string();
            };
            format!("{color}{token}{RESET}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pad a colored string to `width` visible columns. Padding is computed from the
/// `plain` (ANSI-free) string so embedded SGR escapes in `colored` don't corrupt
/// the count. Returns `colored` unchanged when it already meets or exceeds `width`.
fn pad_right_colored(plain: &str, colored: &str, width: usize) -> String {
    let len = plain.chars().count();
    if len >= width {
        colored.to_string()
    } else {
        format!("{colored}{}", " ".repeat(width - len))
    }
}

pub fn pad_right(s: &str, width: usize) -> String {
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
    pub detail: ItemDetail,
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
                emit_line(line);
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
    fn ansi_constants_are_sgr_escapes() {
        assert_eq!(RESET, "\x1b[0m");
        assert_eq!(DIM, "\x1b[2m");
        assert_eq!(BLUE, "\x1b[34m");
        assert_eq!(GREEN, "\x1b[32m");
        assert_eq!(YELLOW, "\x1b[33m");
        assert_eq!(RED, "\x1b[31m");
    }

    #[test]
    fn use_color_is_disabled_without_a_tty() {
        // The test harness runs without an interactive stderr, so color gating
        // is off regardless of NO_COLOR. Guards the `interactive()` precondition.
        assert!(!interactive());
        assert!(!use_color());
    }

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
        assert_eq!(format_duration(3_599_000), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3_600_000), "1h 0m");
        assert_eq!(format_duration(3_900_000), "1h 5m");
        assert_eq!(format_duration(86_340_000), "23h 59m");
    }

    #[test]
    fn colorize_size_disabled_returns_plain() {
        assert_eq!(colorize_size("4 KiB", false), "4 KiB");
        assert_eq!(colorize_size("164.54 MiB", false), "164.54 MiB");
    }

    #[test]
    fn colorize_size_empty_returns_plain() {
        assert_eq!(colorize_size("", true), "");
        assert_eq!(colorize_size("", false), "");
    }

    #[test]
    fn colorize_size_bytes_dim() {
        assert_eq!(colorize_size("512 B", true), format!("{DIM}512 B{RESET}"));
    }

    #[test]
    fn colorize_size_kib_blue() {
        assert_eq!(colorize_size("4 KiB", true), format!("{BLUE}4 KiB{RESET}"));
    }

    #[test]
    fn colorize_size_mib_green() {
        assert_eq!(
            colorize_size("164.54 MiB", true),
            format!("{GREEN}164.54 MiB{RESET}")
        );
    }

    #[test]
    fn colorize_size_gib_yellow() {
        assert_eq!(
            colorize_size("1.2 GiB", true),
            format!("{YELLOW}1.2 GiB{RESET}")
        );
    }

    #[test]
    fn colorize_size_large_units_red() {
        assert_eq!(colorize_size("3 TiB", true), format!("{RED}3 TiB{RESET}"));
        assert_eq!(colorize_size("3 PiB", true), format!("{RED}3 PiB{RESET}"));
        assert_eq!(colorize_size("3 EiB", true), format!("{RED}3 EiB{RESET}"));
    }

    #[test]
    fn colorize_size_unknown_unit_returns_plain() {
        assert_eq!(colorize_size("7 widgets", true), "7 widgets");
        assert_eq!(colorize_size("nonsense", true), "nonsense");
    }

    #[test]
    fn colorize_duration_disabled_returns_plain() {
        assert_eq!(colorize_duration("2m 15s", false), "2m 15s");
        assert_eq!(colorize_duration("42ms", false), "42ms");
    }

    #[test]
    fn colorize_duration_millis_dim() {
        assert_eq!(colorize_duration("42ms", true), format!("{DIM}42ms{RESET}"));
    }

    #[test]
    fn colorize_duration_seconds_green() {
        assert_eq!(
            colorize_duration("9.6s", true),
            format!("{GREEN}9.6s{RESET}")
        );
    }

    #[test]
    fn colorize_duration_minutes_and_seconds() {
        assert_eq!(
            colorize_duration("2m 15s", true),
            format!("{YELLOW}2m{RESET} {GREEN}15s{RESET}")
        );
    }

    #[test]
    fn colorize_duration_hours_and_minutes() {
        assert_eq!(
            colorize_duration("1h 5m", true),
            format!("{RED}1h{RESET} {YELLOW}5m{RESET}")
        );
    }

    #[test]
    fn pad_right_colored_pads_from_plain_width() {
        let plain = "4 KiB";
        let colored = format!("{BLUE}4 KiB{RESET}");
        assert_eq!(
            pad_right_colored(plain, &colored, 10),
            format!("{BLUE}4 KiB{RESET}     ")
        );
    }

    #[test]
    fn pad_right_colored_no_pad_when_at_or_over_width() {
        let plain = "164.54 MiB";
        let colored = format!("{GREEN}164.54 MiB{RESET}");
        assert_eq!(pad_right_colored(plain, &colored, 10), colored);
        assert_eq!(pad_right_colored(plain, &colored, 5), colored);
    }

    #[test]
    fn pad_right_colored_plain_input_matches_pad_right() {
        // With no ANSI in `colored`, the helper behaves like `pad_right`.
        assert_eq!(
            pad_right_colored("4 KiB", "4 KiB", 10),
            pad_right("4 KiB", 10)
        );
    }

    #[test]
    fn format_detail_success() {
        let d = ItemDetail::success("freed", "4 KiB", "2.1s");
        assert_eq!(
            format_detail_colored(&d, false),
            "freed        4 KiB      in 2.1s"
        );
    }

    #[test]
    fn format_detail_no_change_aligns_with_freed() {
        let no_change =
            format_detail_colored(&ItemDetail::success("no change", "124 KiB", "1.1s"), false);
        let freed = format_detail_colored(&ItemDetail::success("freed", "4 KiB", "2.1s"), false);
        let big = format_detail_colored(
            &ItemDetail::success("no change", "164.54 MiB", "1.2s"),
            false,
        );
        let no_change_in = no_change.find(" in ").unwrap();
        let freed_in = freed.find(" in ").unwrap();
        let big_in = big.find(" in ").unwrap();
        assert_eq!(no_change_in, freed_in);
        assert_eq!(no_change_in, big_in);
    }

    #[test]
    fn format_detail_dry_run_drops_duration() {
        let d = ItemDetail::dry_run("would delete", "1 KiB");
        let s = format_detail_colored(&d, false);
        assert!(!s.contains(" in "));
        assert!(s.starts_with("would delete "));
        assert!(s.contains("1 KiB"));
    }

    #[test]
    fn format_detail_failure_renders_suffix() {
        let d = ItemDetail::failure("permission denied (os error 13)");
        let s = format_detail_colored(&d, false);
        assert_eq!(s, "failed       permission denied (os error 13)");
        assert!(!s.contains(" in "));
    }

    #[test]
    fn format_detail_plus_verb() {
        let d = ItemDetail::success("+", "4 KiB", "1.0s");
        let s = format_detail_colored(&d, false);
        assert!(s.starts_with("+           "));
        assert!(s.contains("4 KiB"));
        assert!(s.contains("in 1.0s"));
    }

    #[test]
    fn format_detail_with_fsck_suffix() {
        let d = ItemDetail::success("freed", "4 KiB", "2.1s").with_suffix("; fsck: 3 dangling");
        let s = format_detail_colored(&d, false);
        assert!(s.ends_with("; fsck: 3 dangling"));
        assert!(s.contains("in 2.1s"));
    }

    #[test]
    fn format_detail_colored_preserves_column_positions() {
        // Coloring the amounts must not shift the visible columns: the `" in "`
        // index computed after stripping ANSI escapes must match the plain render.
        let d = ItemDetail::success("freed", "4 KiB", "2.1s");
        let plain = format_detail_colored(&d, false);
        let colored = format_detail_colored(&d, true);

        assert_ne!(plain, colored);
        assert!(colored.contains("\x1b["));

        let stripped: String = strip_ansi(&colored);
        assert_eq!(stripped, plain);
        assert_eq!(stripped.find(" in "), plain.find(" in "));
    }

    /// Remove SGR escape sequences (`\x1b[...m`) so a colored render can be
    /// compared against its plain counterpart.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for inner in chars.by_ref() {
                    if inner == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
