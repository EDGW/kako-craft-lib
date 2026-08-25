//! Versioned explicit catalog metadata and synchronized persistence.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::container::CONTROL_DIR;
use crate::destination::provider::{DESTINATION_FORMAT_VERSION, DESTINATION_METADATA_FILE};

/// Provider kind for an explicit catalog Destination.
pub(super) const CATALOG_KIND: &str = "catalog";

/// Complete `.kcl/destination.json` document for a catalog Destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CatalogMetadata {
    /// Common Destination metadata schema version.
    pub(super) version: u32,
    /// Provider kind, always [`CATALOG_KIND`].
    pub(super) kind: String,
    /// Container specifications indexed by slash-separated logical path.
    pub(super) containers: BTreeMap<String, CatalogContainer>,
    /// Explicit Subcontainer logical paths.
    pub(super) subcontainers: BTreeSet<String>,
}

impl Default for CatalogMetadata {
    fn default() -> Self {
        Self {
            version: DESTINATION_FORMAT_VERSION,
            kind: CATALOG_KIND.to_owned(),
            containers: BTreeMap::new(),
            subcontainers: BTreeSet::new(),
        }
    }
}

/// Persisted specification for one Container member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CatalogContainer {
    /// Concrete Container kind used to initialize or validate the member.
    pub(super) kind: String,
    /// Destination-root-relative or absolute filesystem path.
    pub(super) filesystem_path: PathBuf,
}

/// Loads and validates catalog metadata from a Destination root.
///
/// # Arguments
///
/// * `root` - Root containing `.kcl/destination.json`.
///
/// # Returns
///
/// Parsed current catalog metadata.
///
/// # Errors
///
/// Returns an error for file, JSON, version, provider-kind, or catalog-path failures.
pub(super) fn load(root: &Path) -> Result<CatalogMetadata> {
    let path = metadata_path(root);
    let metadata: CatalogMetadata = serde_json::from_reader(BufReader::new(
        File::open(&path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))?;
    if metadata.version != DESTINATION_FORMAT_VERSION {
        return Err(anyhow!(
            "unsupported destination format version {} in {}",
            metadata.version,
            path.display()
        ));
    }
    if metadata.kind != CATALOG_KIND {
        return Err(anyhow!(
            "{} describes a '{}' destination, not catalog",
            path.display(),
            metadata.kind
        ));
    }
    for path in metadata
        .containers
        .keys()
        .chain(metadata.subcontainers.iter())
    {
        validate_logical_path(path)?;
    }
    Ok(metadata)
}

/// Creates catalog metadata only when it does not already exist.
///
/// # Arguments
///
/// * `root` - Destination root to create.
///
/// # Returns
///
/// `Ok(())` after new metadata is synchronized, or when another creator won the race.
///
/// # Errors
///
/// Returns an error when directories or the initial document cannot be persisted.
pub(super) fn initialize(root: &Path) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("failed to create destination root {}", root.display()))?;
    fs::create_dir_all(root.join(CONTROL_DIR)).with_context(|| {
        format!(
            "failed to create destination control directory {}",
            root.display()
        )
    })?;
    let path = metadata_path(root);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            serde_json::to_writer_pretty(&mut file, &CatalogMetadata::default())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Atomically replaces authoritative catalog metadata.
///
/// # Arguments
///
/// * `root` - Destination root owning the document.
/// * `metadata` - Complete replacement catalog.
///
/// # Returns
///
/// `Ok(())` after a synchronized temporary file is renamed into place.
///
/// # Errors
///
/// Returns an error for serialization, file creation, synchronization, or rename failures.
pub(super) fn save(root: &Path, metadata: &CatalogMetadata) -> Result<()> {
    let path = metadata_path(root);
    let temporary = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, metadata)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Validates a slash-separated logical catalog path.
///
/// # Arguments
///
/// * `path` - Candidate Container or Subcontainer path.
///
/// # Returns
///
/// `Ok(())` when every component is a nonempty ordinary logical name.
///
/// # Errors
///
/// Returns an error for empty, absolute, repeated-separator, dot, colon, or backslash components.
pub(super) fn validate_logical_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
        return Err(anyhow!("invalid logical catalog path: {path:?}"));
    }
    for component in path.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.contains(':')
            || component.contains('\\')
        {
            return Err(anyhow!("invalid logical catalog path: {path:?}"));
        }
    }
    Ok(())
}

/// Returns the authoritative catalog metadata path.
///
/// # Arguments
///
/// * `root` - Destination root.
///
/// # Returns
///
/// `<root>/.kcl/destination.json`.
fn metadata_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join(DESTINATION_METADATA_FILE)
}
