//! Incoming-link metadata schema, key validation, locking, and migration.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::super::{
    CONTROL_DIR, EntryKey, INCOMING_LINKS_FORMAT_VERSION, LinkMetadataMigrationError,
};
use super::filesystem::write_json_atomic;

#[derive(Debug, Serialize, Deserialize)]
/// Versioned metadata containing every reciprocal incoming-link record.
pub(super) struct IncomingLinksMetadata {
    /// On-disk schema version used to select parsing or migration behavior.
    pub(super) version: u32,
    /// Total number of records across all target-entry vectors.
    pub(super) count: u64,
    /// Incoming records grouped by the ordinary target entry key in this container.
    pub(super) links: BTreeMap<EntryKey, Vec<IncomingLink>>,
}

impl Default for IncomingLinksMetadata {
    fn default() -> Self {
        Self {
            version: INCOMING_LINKS_FORMAT_VERSION,
            count: 0,
            links: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
/// Identity of one external outgoing link targeting a local ordinary entry.
pub(super) struct IncomingLink {
    /// Entry key used by the external link container.
    pub(super) linker_key: EntryKey,
    /// Persistent UID of the external link container.
    pub(super) linker_uid: String,
}

/// Validates that an entry key remains inside the container's data namespace.
///
/// # Arguments
///
/// * `key` - Root-relative entry key to validate.
///
/// # Returns
///
/// `Ok(())` for a non-empty relative path made only of normal components and not rooted at `.kcl`;
/// otherwise returns a clone of the rejected key.
pub(crate) fn validate_key(key: &EntryKey) -> std::result::Result<(), EntryKey> {
    let path = Path::new(key);
    if key.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == CONTROL_DIR)
    {
        return Err(key.clone());
    }
    Ok(())
}

/// Opens or creates the persistent file used for advisory container locking.
///
/// # Arguments
///
/// * `path` - Full path to `.kcl/container.lock` for the container.
///
/// # Returns
///
/// A readable and writable file handle suitable for `fs2` locking.
///
/// # Errors
///
/// Returns an error if the lock file cannot be opened or created.
pub(super) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))
}

/// Loads current incoming metadata or safely migrates version-one metadata.
///
/// # Arguments
///
/// * `path` - Full path to the local container's `.kcl/links.json` file.
///
/// # Returns
///
/// Parsed current incoming-link metadata.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed, its version is absent or unsupported,
/// or version-one migration cannot be validated, backed up, or persisted.
pub(super) fn read_incoming_metadata(path: &Path) -> Result<IncomingLinksMetadata> {
    let value: serde_json::Value = serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| incoming_migration_failed(path, "metadata has no numeric version"))?
        as u32;
    match version {
        INCOMING_LINKS_FORMAT_VERSION => serde_json::from_value(value)
            .with_context(|| format!("failed to parse {}", path.display())),
        1 => migrate_incoming_v1(path, value),
        version => Err(LinkMetadataMigrationError::Required {
            path: path.to_owned(),
            version,
        }
        .into()),
    }
}

/// Converts parsed version-one incoming records to the current multi-source schema.
///
/// # Arguments
///
/// * `path` - Existing version-one incoming metadata-file path.
/// * `value` - Parsed version-one JSON whose values may be single objects or arrays.
///
/// # Returns
///
/// Deduplicated current metadata after backup and atomic persistence.
///
/// # Errors
///
/// Returns a typed migration error when the JSON shape or a record is invalid, backup creation
/// fails, or the migrated metadata cannot be persisted.
fn migrate_incoming_v1(path: &Path, value: serde_json::Value) -> Result<IncomingLinksMetadata> {
    let links = value
        .get("links")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| incoming_migration_failed(path, "metadata has no links object"))?;
    let mut migrated = BTreeMap::new();
    let mut duplicate_count = 0usize;
    for (target_key, raw) in links {
        let candidates = if let Some(array) = raw.as_array() {
            array.clone()
        } else if raw.is_object() {
            vec![raw.clone()]
        } else {
            return Err(incoming_migration_failed(
                path,
                format!("incoming value for {target_key} is not an object or array"),
            ));
        };
        let mut sources = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for candidate in candidates {
            let source: IncomingLink = serde_json::from_value(candidate).map_err(|error| {
                incoming_migration_failed(
                    path,
                    format!("invalid incoming record for {target_key}: {error}"),
                )
            })?;
            if seen.insert((source.linker_uid.clone(), source.linker_key.clone())) {
                sources.push(source);
            } else {
                duplicate_count += 1;
            }
        }
        if !sources.is_empty() {
            migrated.insert(target_key.clone(), sources);
        }
    }
    let count = migrated.values().map(Vec::len).sum::<usize>() as u64;
    let metadata = IncomingLinksMetadata {
        version: INCOMING_LINKS_FORMAT_VERSION,
        count,
        links: migrated,
    };
    backup_incoming_v1(path)?;
    write_json_atomic(path, &metadata).map_err(|error| {
        incoming_migration_failed(path, format!("failed to write v2 metadata: {error:#}"))
    })?;
    info!(
        path = %path.display(),
        link_count = count,
        duplicate_count,
        "migrated incoming link metadata to v2"
    );
    Ok(metadata)
}

/// Constructs a typed incoming-metadata migration failure.
///
/// # Arguments
///
/// * `path` - Metadata-file path whose migration failed.
/// * `reason` - Specific parsing, backup, or persistence failure.
///
/// # Returns
///
/// An erased [`LinkMetadataMigrationError::Failed`] retaining `path` and `reason`.
fn incoming_migration_failed(path: &Path, reason: impl Into<String>) -> anyhow::Error {
    LinkMetadataMigrationError::Failed {
        path: path.to_owned(),
        reason: reason.into(),
    }
    .into()
}

/// Creates and synchronizes the stable `.v1.backup` sibling before migration.
///
/// # Arguments
///
/// * `path` - Existing version-one incoming metadata file to copy.
///
/// # Returns
///
/// The backup path, reusing an existing regular-file backup without overwriting it.
///
/// # Errors
///
/// Returns a typed migration error if the filename is unavailable, an existing backup is not a
/// regular file, or copying, opening, or synchronizing the backup fails.
fn backup_incoming_v1(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| incoming_migration_failed(path, "metadata path has no file name"))?;
    let backup = path.with_file_name(format!("{}.v1.backup", file_name.to_string_lossy()));
    if backup.exists() && !backup.is_file() {
        return Err(incoming_migration_failed(
            path,
            format!("backup path is not a file: {}", backup.display()),
        ));
    }
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            incoming_migration_failed(
                path,
                format!("failed to create backup {}: {error}", backup.display()),
            )
        })?;
        File::open(&backup)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                incoming_migration_failed(
                    path,
                    format!("failed to sync backup {}: {error}", backup.display()),
                )
            })?;
    }
    Ok(backup)
}
