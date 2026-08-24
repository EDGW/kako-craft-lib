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
pub(super) struct IncomingLinksMetadata {
    pub(super) version: u32,
    pub(super) count: u64,
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
pub(super) struct IncomingLink {
    pub(super) linker_key: EntryKey,
    pub(super) linker_uid: String,
}

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

pub(super) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))
}

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

fn incoming_migration_failed(path: &Path, reason: impl Into<String>) -> anyhow::Error {
    LinkMetadataMigrationError::Failed {
        path: path.to_owned(),
        reason: reason.into(),
    }
    .into()
}

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
