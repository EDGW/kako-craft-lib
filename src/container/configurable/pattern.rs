//! Positive gitignore-style pattern validation and matching.

use std::fs::{self, OpenOptions};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use ignore::gitignore::GitignoreBuilder;
use uuid::Uuid;

/// Validates one supported positive pattern.
///
/// # Arguments
///
/// * `pattern` - JSON rule pattern.
///
/// # Returns
///
/// `Ok(())` when the pattern can be compiled.
///
/// # Errors
///
/// Returns an error for empty, negative, comment, or invalid gitignore patterns.
pub(super) fn validate(pattern: &str) -> Result<()> {
    if pattern.is_empty() || pattern.starts_with('!') || pattern.starts_with('#') {
        return Err(anyhow!("unsupported configurable pattern: {pattern:?}"));
    }
    matcher(Path::new("."), pattern, false).map(|_| ())
}

/// Tests one entry key against one positive pattern.
///
/// # Arguments
///
/// * `root` - Container root used to select filesystem case behavior.
/// * `pattern` - Valid positive gitignore-style pattern.
/// * `key` - Root-relative entry key.
///
/// # Returns
///
/// `true` when the entry or one of its directory parents matches.
///
/// # Errors
///
/// Returns an error when matcher construction fails.
pub(super) fn matches(root: &Path, pattern: &str, key: &str) -> Result<bool> {
    let matcher = matcher(root, pattern, filesystem_case_insensitive(root)?)?;
    Ok(matcher
        .matched_path_or_any_parents(Path::new(key), false)
        .is_ignore())
}

/// Builds one single-pattern matcher so rule order remains first-match-wins.
///
/// # Arguments
///
/// * `root` - Pattern root passed to the gitignore matcher.
/// * `pattern` - Positive pattern to compile.
/// * `case_insensitive` - Whether matching follows case-insensitive filesystem semantics.
///
/// # Returns
///
/// A compiled matcher containing exactly this pattern.
///
/// # Errors
///
/// Returns an error when case configuration or pattern compilation fails.
fn matcher(
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<ignore::gitignore::Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    builder
        .case_insensitive(case_insensitive)
        .context("failed to configure pattern case sensitivity")?;
    builder
        .add_line(None, pattern)
        .with_context(|| format!("invalid configurable pattern {pattern:?}"))?;
    builder
        .build()
        .context("failed to build configurable pattern")
}

/// Detects the actual case behavior of the Container's filesystem.
///
/// # Arguments
///
/// * `root` - Existing ConfigurableContainer root whose `.kcl` directory hosts a probe.
///
/// # Returns
///
/// `true` when differently cased spellings address the same probe file; otherwise `false`.
///
/// # Errors
///
/// Returns an error when the isolated probe cannot be created, inspected, or removed.
fn filesystem_case_insensitive(root: &Path) -> Result<bool> {
    let control = root.join(crate::container::CONTROL_DIR);
    let identifier = Uuid::new_v4().simple().to_string();
    let mixed = control.join(format!(".case-probe-A{identifier}"));
    let alternate = control.join(format!(".case-probe-a{identifier}"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&mixed)
        .with_context(|| format!("failed to create filesystem case probe {}", mixed.display()))?;
    drop(file);
    let insensitive = alternate.exists();
    fs::remove_file(&mixed)
        .with_context(|| format!("failed to remove filesystem case probe {}", mixed.display()))?;
    Ok(insensitive)
}
