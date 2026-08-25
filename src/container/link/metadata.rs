//! Outgoing-link metadata schema, loading, and version-one migration.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::super::{
    EntryKey, LinkMetadataMigrationError, OUTGOING_LINKS_FORMAT_VERSION, open_container,
};
use super::filesystem::{absolute_path, recorded_container_path, write_json_atomic};

#[derive(Debug, Serialize, Deserialize)]
/// Versioned metadata owned by a link container for all outgoing relationships.
pub(super) struct OutgoingLinksMetadata {
    /// On-disk schema version used to select parsing or migration behavior.
    pub(super) version: u32,
    #[serde(default = "default_prefer_relative")]
    /// Container-wide default controlling relative recorded paths and symbolic-link targets.
    pub(super) prefer_relative: bool,
    /// Outgoing records indexed by the linker entry key materialized in this container.
    pub(super) links: BTreeMap<EntryKey, OutgoingLink>,
}

impl Default for OutgoingLinksMetadata {
    fn default() -> Self {
        Self {
            version: OUTGOING_LINKS_FORMAT_VERSION,
            prefer_relative: true,
            links: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Persisted identity and location of one outgoing link target.
pub(super) struct OutgoingLink {
    /// Entry key in the target container.
    pub(super) target_key: EntryKey,
    /// Persistent UID expected from the target container at access time.
    pub(super) container_uid: String,
    /// Target container path, absolute or relative to the owning link-container root.
    pub(super) container_path: PathBuf,
}

/// Supplies the relative-path preference when older JSON omits the field.
///
/// # Returns
///
/// `true`, preserving the default of preferring relative paths.
fn default_prefer_relative() -> bool {
    true
}

/// Loads current outgoing metadata or safely migrates version-one metadata.
///
/// # Arguments
///
/// * `path` - Full path to the owning link container's `outgoing-links.json` file.
///
/// # Returns
///
/// Parsed current metadata, or empty default metadata when the file does not exist.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed, its version is absent or unsupported,
/// or a required version-one migration cannot be verified, backed up, or persisted.
pub(super) fn read_metadata(path: &Path) -> Result<OutgoingLinksMetadata> {
    if !path.exists() {
        return Ok(OutgoingLinksMetadata::default());
    }
    let value: serde_json::Value = serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| migration_failed(path, "metadata has no numeric version"))?
        as u32;
    match version {
        OUTGOING_LINKS_FORMAT_VERSION => serde_json::from_value(value)
            .with_context(|| format!("failed to parse {}", path.display())),
        1 => migrate_outgoing_v1(path, value),
        version => Err(LinkMetadataMigrationError::Required {
            path: path.to_owned(),
            version,
        }
        .into()),
    }
}

/// Converts parsed version-one outgoing JSON to the current container-path schema.
///
/// # Arguments
///
/// * `path` - Existing version-one metadata-file path, also used to derive its container root.
/// * `value` - Parsed version-one JSON object to validate and convert.
///
/// # Returns
///
/// Current metadata after its target UIDs are verified, the source is backed up, and the migrated
/// document is atomically persisted.
///
/// # Errors
///
/// Returns a typed migration error for malformed records, unverifiable target paths or UIDs,
/// unsafe backup state, or failure to persist the migrated document.
fn migrate_outgoing_v1(path: &Path, value: serde_json::Value) -> Result<OutgoingLinksMetadata> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| migration_failed(path, "metadata path has no container root"))?;
    let prefer_relative = value
        .get("prefer_relative")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let links = value
        .get("links")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| migration_failed(path, "metadata has no links object"))?;
    let mut migrated = BTreeMap::new();
    for (linker_key, raw) in links {
        let target_key = raw
            .get("target_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| migration_failed(path, format!("{linker_key} has no target_key")))?
            .to_owned();
        let container_uid = raw
            .get("container_uid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| migration_failed(path, format!("{linker_key} has no container_uid")))?
            .to_owned();
        let container_path = if let Some(recorded) = raw.get("container_path") {
            serde_json::from_value::<PathBuf>(recorded.clone()).map_err(|error| {
                migration_failed(
                    path,
                    format!("invalid container_path for {linker_key}: {error}"),
                )
            })?
        } else {
            let target_filename = raw
                .get("target_filename")
                .cloned()
                .ok_or_else(|| {
                    migration_failed(
                        path,
                        format!("{linker_key} has neither container_path nor target_filename"),
                    )
                })
                .and_then(|value| {
                    serde_json::from_value::<PathBuf>(value).map_err(|error| {
                        migration_failed(
                            path,
                            format!("invalid target_filename for {linker_key}: {error}"),
                        )
                    })
                })?;
            let target_key_path = Path::new(&target_key);
            if !target_filename.ends_with(target_key_path) {
                return Err(migration_failed(
                    path,
                    format!(
                        "target_filename {} does not end with target key {}",
                        target_filename.display(),
                        target_key
                    ),
                ));
            }
            let mut target_root = target_filename.clone();
            for _ in target_key_path.components() {
                if !target_root.pop() {
                    return Err(migration_failed(
                        path,
                        format!("cannot derive target root for {linker_key}"),
                    ));
                }
            }
            let old_prefer_relative = raw
                .get("prefer_relative")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(prefer_relative);
            recorded_container_path(
                &absolute_path(root)?,
                &absolute_path(&target_root)?,
                old_prefer_relative,
            )
        };
        let resolved = if container_path.is_absolute() {
            container_path.clone()
        } else {
            root.join(&container_path)
        };
        let actual_uid = open_container(&resolved)
            .and_then(|container| container.uid())
            .map_err(|error| {
                migration_failed(
                    path,
                    format!(
                        "cannot verify target container for {linker_key} at {}: {error:#}",
                        resolved.display()
                    ),
                )
            })?;
        if actual_uid != container_uid {
            return Err(migration_failed(
                path,
                format!(
                    "target UID mismatch for {linker_key}: recorded {container_uid}, found {actual_uid}"
                ),
            ));
        }
        migrated.insert(
            linker_key.clone(),
            OutgoingLink {
                target_key,
                container_uid,
                container_path,
            },
        );
    }
    let metadata = OutgoingLinksMetadata {
        version: OUTGOING_LINKS_FORMAT_VERSION,
        prefer_relative,
        links: migrated,
    };
    backup_v1(path)?;
    write_json_atomic(path, &metadata).map_err(|error| {
        migration_failed(path, format!("failed to write v2 metadata: {error:#}"))
    })?;
    info!(path = %path.display(), link_count = metadata.links.len(), "migrated outgoing link metadata to v2");
    Ok(metadata)
}

/// Constructs a typed outgoing-metadata migration failure.
///
/// # Arguments
///
/// * `path` - Metadata-file path whose migration failed.
/// * `reason` - Specific validation, backup, or persistence failure.
///
/// # Returns
///
/// An erased [`LinkMetadataMigrationError::Failed`] retaining `path` and `reason`.
fn migration_failed(path: &Path, reason: impl Into<String>) -> anyhow::Error {
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
/// * `path` - Existing version-one outgoing metadata file to copy.
///
/// # Returns
///
/// The backup path, reusing an existing regular-file backup without overwriting it.
///
/// # Errors
///
/// Returns a typed migration error if the filename is unavailable, an existing backup is not a
/// regular file, or copying, opening, or synchronizing the backup fails.
fn backup_v1(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| migration_failed(path, "metadata path has no file name"))?;
    let backup = path.with_file_name(format!("{}.v1.backup", file_name.to_string_lossy()));
    if backup.exists() && !backup.is_file() {
        return Err(migration_failed(
            path,
            format!("backup path is not a file: {}", backup.display()),
        ));
    }
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            migration_failed(
                path,
                format!("failed to create backup {}: {error}", backup.display()),
            )
        })?;
        File::open(&backup)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                migration_failed(
                    path,
                    format!("failed to sync backup {}: {error}", backup.display()),
                )
            })?;
    }
    Ok(backup)
}
