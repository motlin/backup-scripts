use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};
use tracing::{Instrument, info, info_span};

use super::CommandSummary;
use crate::config::{BackblazeExclusionsConfig, expand_tilde};

pub const DEFAULT_FILE: &str = "/Library/Backblaze.bzpkg/bzdata/bzexcluderules_editable.xml";
const BLOCK_START: &str = "<!-- backup-scripts managed development exclusions: begin -->";
const BLOCK_END: &str = "<!-- backup-scripts managed development exclusions: end -->";
const CLOSING_TAG: &str = "</bzexclusions>";
const MANAGED_CONTAINS: &[&str] = &[
    "/.rustup/",
    "/go/pkg/mod/",
    "/.cargo/registry/",
    "/__pycache__/",
    "/.venv/",
    "/.expo/",
];
const RETIRED_CONTAINS: &[&str] = &["/.gem/"];

#[derive(ClapArgs, Debug, Default)]
pub struct Args {
    /// Editable Backblaze exclusion XML.
    /// [config: `backblaze_exclusions.file`, default: Backblaze bzdata path]
    #[arg(long)]
    file: Option<PathBuf>,

    /// Apply the managed exclusion block. Without this flag the command only
    /// reports whether the file needs changes.
    #[arg(long)]
    apply: bool,
}

pub async fn run(
    args: Args,
    cfg: &BackblazeExclusionsConfig,
    dry_run: bool,
) -> Result<CommandSummary> {
    let file = args
        .file
        .or_else(|| cfg.file.clone())
        .map_or_else(|| PathBuf::from(DEFAULT_FILE), |path| expand_tilde(&path));
    let apply = args.apply || cfg.apply.unwrap_or(false);

    async move {
        let original = std::fs::read_to_string(&file)
            .with_context(|| format!("reading Backblaze exclusions {}", file.display()))?;
        let home = std::env::var("HOME").context("HOME is required for Expo exclusion safety")?;
        let updated = reconcile_exclusions(&original, Path::new(&home))?;
        if updated == original {
            info!("Backblaze development exclusions are current");
            return Ok(CommandSummary::ok_one());
        }

        if dry_run || !apply {
            let reason = if dry_run { "dry run" } else { "no --apply" };
            info!(
                file = %file.display(),
                "{reason}: Backblaze development exclusions need an update"
            );
            return Ok(CommandSummary::ok_one());
        }

        let backup = backup_path(&file)?;
        std::fs::copy(&file, &backup).with_context(|| {
            format!(
                "backing up Backblaze exclusions {} to {}",
                file.display(),
                backup.display()
            )
        })?;
        std::fs::write(&file, updated)
            .with_context(|| format!("writing Backblaze exclusions {}", file.display()))?;
        info!(
            file = %file.display(),
            backup = %backup.display(),
            "Backblaze development exclusions updated"
        );
        Ok(CommandSummary::ok_one())
    }
    .instrument(info_span!("backblaze-exclusions", apply))
    .await
}

fn reconcile_exclusions(original: &str, home: &Path) -> Result<String> {
    if original.matches(CLOSING_TAG).count() != 1 {
        bail!("Backblaze exclusions must contain exactly one {CLOSING_TAG}");
    }

    let without_block = remove_managed_block(original)?;
    let mut retained = Vec::new();
    for line in without_block.lines() {
        let managed_legacy = MANAGED_CONTAINS
            .iter()
            .chain(RETIRED_CONTAINS)
            .any(|value| line.contains(&format!("contains_1=\"{value}\"")));
        if !managed_legacy {
            retained.push(line);
        }
    }

    let base = retained.join("\n");
    let block = managed_block(home);
    let mut updated = base.replacen(CLOSING_TAG, &format!("{block}\n{CLOSING_TAG}"), 1);
    if original.ends_with('\n') {
        updated.push('\n');
    }
    Ok(updated)
}

fn remove_managed_block(original: &str) -> Result<String> {
    match (original.find(BLOCK_START), original.find(BLOCK_END)) {
        (None, None) => Ok(original.to_string()),
        (Some(start), Some(end)) if start < end => {
            let after = end + BLOCK_END.len();
            let mut output = String::with_capacity(original.len());
            output.push_str(original[..start].trim_end_matches('\n'));
            output.push('\n');
            output.push_str(original[after..].trim_start_matches('\n'));
            Ok(output)
        }
        _ => bail!("Backblaze managed exclusion block markers are unbalanced"),
    }
}

fn managed_block(home: &Path) -> String {
    let home_expo = escape_xml_attribute(&format!(
        "{}/.expo/",
        home.to_string_lossy().to_ascii_lowercase()
    ));
    let standard = MANAGED_CONTAINS
        .iter()
        .filter(|contains| **contains != "/.expo/")
        .map(|contains| exclusion_rule(contains, "*"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{BLOCK_START}\n\
         <!-- Regenerable development data; keep installed gems and home-level Expo state. -->\n\
         {standard}\n\
         {}\n\
         {BLOCK_END}",
        exclusion_rule("/.expo/", &home_expo)
    )
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn exclusion_rule(contains: &str, does_not_contain: &str) -> String {
    format!(
        "<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" \
         skipFirstCharThenStartsWith=\"users/\" contains_1=\"{contains}\" contains_2=\"*\" \
         doesNotContain=\"{does_not_contain}\" endsWith=\"*\" hasFileExtension=\"*\" />"
    )
}

fn backup_path(file: &Path) -> Result<PathBuf> {
    let name = file.file_name().with_context(|| {
        format!(
            "Backblaze exclusion path has no file name: {}",
            file.display()
        )
    })?;
    let mut backup_name = name.to_os_string();
    backup_name.push(".backup-scripts.bak");
    Ok(file.with_file_name(backup_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "\
<?xml version=\"1.0\"?>
<bzexclusions>
<excludefname_rule plat=\"mac\" contains_1=\"/keep-me/\" />
<excludefname_rule plat=\"mac\" contains_1=\"/.gem/\" />
<excludefname_rule plat=\"mac\" contains_1=\"/.expo/\" />
</bzexclusions>
";

    #[test]
    fn reconciles_rules_and_preserves_unmanaged_content() {
        let updated = reconcile_exclusions(ORIGINAL, Path::new("/Users/alice"))
            .expect("exclusions are reconciled");

        assert_eq!(
            updated,
            "\
<?xml version=\"1.0\"?>
<bzexclusions>
<excludefname_rule plat=\"mac\" contains_1=\"/keep-me/\" />
<!-- backup-scripts managed development exclusions: begin -->
<!-- Regenerable development data; keep installed gems and home-level Expo state. -->
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/.rustup/\" contains_2=\"*\" doesNotContain=\"*\" endsWith=\"*\" hasFileExtension=\"*\" />
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/go/pkg/mod/\" contains_2=\"*\" doesNotContain=\"*\" endsWith=\"*\" hasFileExtension=\"*\" />
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/.cargo/registry/\" contains_2=\"*\" doesNotContain=\"*\" endsWith=\"*\" hasFileExtension=\"*\" />
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/__pycache__/\" contains_2=\"*\" doesNotContain=\"*\" endsWith=\"*\" hasFileExtension=\"*\" />
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/.venv/\" contains_2=\"*\" doesNotContain=\"*\" endsWith=\"*\" hasFileExtension=\"*\" />
<excludefname_rule plat=\"mac\" osVers=\"*\" ruleIsOptional=\"t\" skipFirstCharThenStartsWith=\"users/\" contains_1=\"/.expo/\" contains_2=\"*\" doesNotContain=\"/users/alice/.expo/\" endsWith=\"*\" hasFileExtension=\"*\" />
<!-- backup-scripts managed development exclusions: end -->
</bzexclusions>
"
        );
    }

    #[test]
    fn reconciliation_is_idempotent() {
        let first = reconcile_exclusions(ORIGINAL, Path::new("/Users/alice"))
            .expect("first reconciliation succeeds");

        assert_eq!(
            reconcile_exclusions(&first, Path::new("/Users/alice"))
                .expect("second reconciliation succeeds"),
            first
        );
    }

    #[test]
    fn rejects_missing_closing_tag() {
        assert_eq!(
            reconcile_exclusions("<bzexclusions>", Path::new("/Users/alice"))
                .expect_err("missing closing tag is rejected")
                .to_string(),
            "Backblaze exclusions must contain exactly one </bzexclusions>"
        );
    }

    #[test]
    fn escapes_home_directory_for_xml_attribute() {
        assert_eq!(
            escape_xml_attribute("/users/alice & bob/\"project\"/.expo/"),
            "/users/alice &amp; bob/&quot;project&quot;/.expo/"
        );
    }
}
