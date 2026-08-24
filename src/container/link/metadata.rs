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
pub(super) struct OutgoingLinksMetadata {
    pub(super) version: u32,
    #[serde(default = "default_prefer_relative")]
    pub(super) prefer_relative: bool,
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
pub(super) struct OutgoingLink {
    pub(super) target_key: EntryKey,
    pub(super) container_uid: String,
    pub(super) container_path: PathBuf,
}

fn default_prefer_relative() -> bool {
    true
}

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

fn migration_failed(path: &Path, reason: impl Into<String>) -> anyhow::Error {
    LinkMetadataMigrationError::Failed {
        path: path.to_owned(),
        reason: reason.into(),
    }
    .into()
}

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
