//! ConfigurableContainer rule metadata and synchronized persistence.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::container::CONTROL_DIR;
use crate::locator::ContainerLocator;

/// Current `.kcl/link-matches.json` schema version.
const RULES_FORMAT_VERSION: u32 = 1;
/// Rule metadata filename below a ConfigurableContainer control directory.
const RULES_FILE: &str = "link-matches.json";

/// One first-match-wins automatic storage rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRule {
    /// Gitignore-style positive pattern matched against an entry key.
    pub pattern: String,
    /// Target Container locator and stable identity.
    pub container: ConfigRuleTarget,
    /// Routing strategy. Version one accepts only the literal `sha1`.
    pub rule: String,
}

impl ConfigRule {
    /// Creates a SHA-1 content-addressed routing rule.
    ///
    /// # Arguments
    ///
    /// * `pattern` - Positive gitignore-style entry pattern.
    /// * `locator` - Target Container locator resolved relative to the configurable root.
    /// * `uid` - Stable UID expected from the resolved target.
    ///
    /// # Returns
    ///
    /// A version-one `sha1` rule.
    pub fn sha1(
        pattern: impl Into<String>,
        locator: ContainerLocator,
        uid: impl Into<String>,
    ) -> Self {
        Self {
            pattern: pattern.into(),
            container: ConfigRuleTarget {
                locator,
                uid: uid.into(),
            },
            rule: "sha1".to_owned(),
        }
    }
}

/// Persisted target identity for one routing rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRuleTarget {
    /// Target Container locator.
    pub locator: ContainerLocator,
    /// Stable target Container UID verified when rules are saved and used.
    pub uid: String,
}

/// Complete versioned rule metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ConfigRulesMetadata {
    /// On-disk schema version.
    pub(super) version: u32,
    /// Strongly ordered first-match-wins rules.
    pub(super) matches: Vec<ConfigRule>,
}

impl ConfigRulesMetadata {
    /// Returns an empty current-version document.
    fn empty() -> Self {
        Self {
            version: RULES_FORMAT_VERSION,
            matches: Vec::new(),
        }
    }
}

/// Resolves the rule metadata path below a Container root.
///
/// # Arguments
///
/// * `root` - ConfigurableContainer filesystem root.
///
/// # Returns
///
/// `<root>/.kcl/link-matches.json` without filesystem access.
pub(super) fn rules_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join(RULES_FILE)
}

/// Creates an empty rules document only when absent.
///
/// # Arguments
///
/// * `path` - Full `link-matches.json` path.
///
/// # Returns
///
/// `Ok(())` after the current or newly created document validates.
///
/// # Errors
///
/// Returns an error for directory, serialization, synchronization, or validation failures.
pub(super) fn initialize_rules(path: &Path) -> Result<()> {
    if !path.exists() {
        save_rules(path, &ConfigRulesMetadata::empty())?;
    }
    load_rules(path).map(|_| ())
}

/// Loads and validates rule metadata.
///
/// # Arguments
///
/// * `path` - Full `link-matches.json` path.
///
/// # Returns
///
/// A current-version ordered rule document.
///
/// # Errors
///
/// Returns an error for file/JSON failures, unsupported versions or rules,
/// empty UIDs, negative/comment patterns, or invalid pattern syntax.
pub(super) fn load_rules(path: &Path) -> Result<ConfigRulesMetadata> {
    let metadata: ConfigRulesMetadata = serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))?;
    validate_rules(&metadata)?;
    Ok(metadata)
}

/// Atomically persists a complete validated rule document.
///
/// # Arguments
///
/// * `path` - Full `link-matches.json` path.
/// * `metadata` - Complete replacement metadata.
///
/// # Returns
///
/// `Ok(())` after a synchronized temporary file replaces the authoritative document.
///
/// # Errors
///
/// Returns an error for validation, serialization, file, synchronization, or rename failures.
pub(super) fn save_rules(path: &Path, metadata: &ConfigRulesMetadata) -> Result<()> {
    validate_rules(metadata)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".link-matches.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, metadata)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist {}", path.display()))
}

/// Validates metadata fields and every supported positive pattern.
///
/// # Arguments
///
/// * `metadata` - Candidate complete rule document.
///
/// # Returns
///
/// `Ok(())` only when the document is safe to match and resolve.
///
/// # Errors
///
/// Returns an error for unsupported schema/rule values, empty UIDs, forbidden
/// negative/comment forms, or invalid gitignore pattern syntax.
fn validate_rules(metadata: &ConfigRulesMetadata) -> Result<()> {
    if metadata.version != RULES_FORMAT_VERSION {
        return Err(anyhow!(
            "unsupported configurable rule format version {}",
            metadata.version
        ));
    }
    for rule in &metadata.matches {
        if rule.rule != "sha1" {
            return Err(anyhow!("unsupported configurable rule: {}", rule.rule));
        }
        if rule.container.uid.is_empty() {
            return Err(anyhow!("configurable rule target UID cannot be empty"));
        }
        super::pattern::validate(&rule.pattern)?;
    }
    Ok(())
}

/// Constructs current metadata from an ordered public rule list.
///
/// # Arguments
///
/// * `rules` - Complete first-match-wins rules.
///
/// # Returns
///
/// A current-version document.
pub(super) fn from_rules(rules: Vec<ConfigRule>) -> ConfigRulesMetadata {
    ConfigRulesMetadata {
        version: RULES_FORMAT_VERSION,
        matches: rules,
    }
}
