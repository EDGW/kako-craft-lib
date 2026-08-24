use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace};
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTAINER_FORMAT_VERSION, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, CopyError, EntryKey, INCOMING_LINKS_FORMAT_VERSION, LinkFromError,
    LinkInfo, LinkMetadataMigrationError, RemoveError, RenameError, UnlinkError, WriteError,
    WriterError, metadata_path,
};

const LINKS_FILE: &str = "links.json";
const LOCK_FILE: &str = "container.lock";

/// A container backed by a directory on the local filesystem.
///
/// Container entries live below `path`; implementation metadata is kept in
/// `path/.kcl`. Local containers accept incoming links but never create
/// `LinkTo` records.
pub struct LocalContainer {
    path: PathBuf,
    metadata: ContainerMetadata,
    logger: ContainerLogger,
}

impl LocalContainer {
    /// Opens a local container, creating it when necessary.
    ///
    /// A new container uses the directory name as its logical name. Its UID
    /// is initialized while opening so the container logger always has a
    /// stable, traceable identity.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let logical_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("local")
            .to_owned();
        Self::open(path, logical_name, "local")
    }

    /// Opens or creates a container with a caller-selected logical name.
    /// The supplied name is only used when the container is first created.
    pub fn with_logical_name(
        path: impl Into<PathBuf>,
        logical_name: impl Into<String>,
    ) -> Result<Self> {
        Self::open(path.into(), logical_name.into(), "local")
    }

    /// Opens an existing local container from already parsed common metadata.
    pub fn from_metadata(path: impl Into<PathBuf>, metadata: ContainerMetadata) -> Result<Self> {
        Self::from_metadata_as(path.into(), metadata, "local")
    }

    pub fn path(&self) -> &Path {
        let span = container_operation_span!(self.logger, "path");
        let _entered = span.enter();
        trace!(path = %self.path.display(), "getting container path");
        &self.path
    }

    pub(crate) fn path_ref(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open_as(path: PathBuf, logical_name: String, kind: &'static str) -> Result<Self> {
        Self::open(path, logical_name, kind)
    }

    fn open(path: PathBuf, logical_name: String, kind: &'static str) -> Result<Self> {
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create container directory {}", path.display()))?;
        fs::create_dir_all(path.join(CONTROL_DIR)).with_context(|| {
            format!(
                "failed to create container control directory for {}",
                path.display()
            )
        })?;

        let metadata_path = metadata_path(&path);
        let initial = ContainerMetadata {
            version: CONTAINER_FORMAT_VERSION,
            uid: Uuid::new_v4().to_string(),
            logical_name,
            kind: kind.to_owned(),
        };

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&metadata_path)
        {
            Ok(file) => write_json_to(file, &initial)
                .with_context(|| format!("failed to initialize {}", metadata_path.display()))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", metadata_path.display()));
            }
        }

        let metadata = ContainerMetadata::from_file(&metadata_path)?;
        Self::from_metadata_as(path, metadata, kind)
    }

    pub(crate) fn from_metadata_as(
        path: PathBuf,
        metadata: ContainerMetadata,
        expected_kind: &'static str,
    ) -> Result<Self> {
        metadata.validate(&metadata_path(&path))?;
        if metadata.kind != expected_kind {
            return Err(anyhow!(
                "{} describes a '{}' container, not a '{}' container",
                metadata_path(&path).display(),
                metadata.kind,
                expected_kind
            ));
        }
        if !path.is_dir() {
            return Err(anyhow!(
                "container directory does not exist: {}",
                path.display()
            ));
        }

        let logger = ContainerLogger::new(expected_kind, metadata.uid.clone());
        debug!(
            logger = %logger.name(),
            container_uid = %logger.uid(),
            container_kind = logger.kind(),
            operation = "open",
            path = %path.display(),
            "opened container"
        );

        Ok(Self {
            path,
            metadata,
            logger,
        })
    }

    fn lock_path(&self) -> PathBuf {
        self.path.join(CONTROL_DIR).join(LOCK_FILE)
    }

    fn links_path(&self) -> PathBuf {
        self.path.join(CONTROL_DIR).join(LINKS_FILE)
    }

    fn entry_path(&self, key: &EntryKey) -> std::result::Result<PathBuf, EntryKey> {
        validate_key(key)?;
        Ok(self.path.join(key))
    }
}

impl Container for LocalContainer {
    fn root_path(&self) -> PathBuf {
        self.path.clone()
    }

    fn metadata(&self) -> ContainerMetadata {
        let span = container_operation_span!(self.logger, "metadata");
        let _entered = span.enter();
        trace!("getting container metadata");
        self.metadata.clone()
    }

    fn uid(&self) -> Result<String> {
        let span = container_operation_span!(self.logger, "uid");
        let _entered = span.enter();
        trace!("getting container UID");
        Ok(self.metadata.uid.clone())
    }

    fn logical_name(&self) -> String {
        let span = container_operation_span!(self.logger, "logical_name");
        let _entered = span.enter();
        trace!("getting container logical name");
        self.metadata.logical_name.clone()
    }

    fn kind(&self) -> String {
        let span = container_operation_span!(self.logger, "kind");
        let _entered = span.enter();
        trace!("getting container kind");
        self.metadata.kind.clone()
    }

    fn list(&self) -> Result<Vec<EntryKey>> {
        let span = container_operation_span!(self.logger, "list");
        let _entered = span.enter();
        debug!(path = %self.path.display(), "listing container entries");
        let mut entries = Vec::new();
        list_entries(&self.path, &self.path, &mut entries)?;
        entries.sort();
        debug!(entry_count = entries.len(), "listed container entries");
        Ok(entries)
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        let span = container_operation_span!(self.logger, "filepath");
        let _entered = span.enter();
        debug!(entry_key = %key, "resolving entry path");
        self.entry_path(key)
            .map_err(|key| anyhow!("invalid entry key: {key}"))
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let span = container_operation_span!(self.logger, "read");
        let _entered = span.enter();
        debug!(entry_key = %key, "reading container entry");
        let path = self.filepath(key)?;
        fs::read(&path).with_context(|| format!("failed to read entry {}", path.display()))
    }

    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError> {
        let span = container_operation_span!(self.logger, "writer");
        let _entered = span.enter();
        debug!("creating container writer");
        let lock_file = open_lock_file(&self.lock_path()).map_err(WriterError::Other)?;
        match lock_file.try_lock_exclusive() {
            Ok(()) => trace!(path = %self.lock_path().display(), "acquired file lock"),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                debug!(path = %self.lock_path().display(), "file lock is held by another writer");
                return Err(WriterError::ContainerLocked);
            }
            Err(error) => return Err(WriterError::Other(error.into())),
        }

        Ok(Box::new(LocalContainerWriteGuard {
            root: self.path.clone(),
            links_path: self.links_path(),
            logger: self.logger.clone(),
            _lock_file: lock_file,
        }))
    }
}

pub struct LocalContainerWriteGuard {
    root: PathBuf,
    links_path: PathBuf,
    logger: ContainerLogger,
    // Keeping the descriptor alive keeps the OS-level advisory lock held.
    // The lock is automatically released on Drop and on process termination.
    _lock_file: File,
}

impl LocalContainerWriteGuard {
    fn entry_path(&self, key: &EntryKey) -> std::result::Result<PathBuf, EntryKey> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    fn links(&self) -> Result<IncomingLinksMetadata> {
        if !self.links_path.exists() {
            return Ok(IncomingLinksMetadata::default());
        }
        let links = read_incoming_metadata(&self.links_path)?;
        let actual_count = links.links.values().map(Vec::len).sum::<usize>() as u64;
        if links.count != actual_count {
            return Err(anyhow!(
                "invalid link count in {}: recorded {}, actual {}",
                self.links_path.display(),
                links.count,
                actual_count
            ));
        }
        Ok(links)
    }

    fn save_links(&self, links: &mut IncomingLinksMetadata) -> Result<()> {
        links.count = links.links.values().map(Vec::len).sum::<usize>() as u64;
        write_json_atomic(&self.links_path, links)
    }

    fn is_linked_from(&self, key: &EntryKey) -> Result<bool> {
        Ok(self.links()?.links.contains_key(key))
    }
}

impl ContainerWriteGuard for LocalContainerWriteGuard {
    fn container_uid(&self) -> &str {
        self.logger.uid()
    }

    fn link_snapshot(&self) -> Result<super::ContainerLinkSnapshot> {
        let links = self.links()?;
        let incoming = links
            .links
            .into_iter()
            .flat_map(|(target_key, sources)| {
                sources
                    .into_iter()
                    .map(move |source| super::IncomingLinkRecord {
                        target_key: target_key.clone(),
                        linker_uid: source.linker_uid,
                        linker_key: source.linker_key,
                    })
            })
            .collect();
        Ok(super::ContainerLinkSnapshot {
            container_uid: self.logger.uid().to_owned(),
            container_path: self.root.clone(),
            incoming,
            outgoing: Vec::new(),
        })
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.entry_path(key)
            .map_err(|key| anyhow!("invalid entry key: {key}"))
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let path = self.filepath(key)?;
        fs::read(&path).with_context(|| format!("failed to read entry {}", path.display()))
    }

    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        let span = container_operation_span!(self.logger, "add");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "adding container entry");
        let path = self.entry_path(key).map_err(AddError::InvalidEntryKey)?;
        if self.is_linked_from(key).map_err(AddError::Other)? {
            return Err(AddError::EntryExists(key.clone()));
        }
        match write_entry_atomic(&self.root, &path, &data, false) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(AddError::EntryExists(key.clone()));
            }
            Err(error) => return Err(AddError::Other(error.into())),
        }
        info!(entry_key = %key, byte_count = data.len(), "added container entry");
        Ok(())
    }

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError> {
        let span = container_operation_span!(self.logger, "write");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "writing container entry");
        let path = self.entry_path(key).map_err(WriteError::InvalidEntryKey)?;
        let byte_count = data.len();
        write_entry_atomic(&self.root, &path, &data, true)
            .map_err(|error| WriteError::Other(error.into()))?;
        info!(entry_key = %key, byte_count, "wrote container entry");
        Ok(())
    }

    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError> {
        let span = container_operation_span!(self.logger, "remove");
        let _entered = span.enter();
        debug!(entry_key = %key, "removing container entry");
        let path = self.entry_path(key).map_err(RemoveError::InvalidEntryKey)?;
        if self.is_linked_from(key).map_err(RemoveError::Other)? {
            return Err(RemoveError::EntryIsLinked(key.clone()));
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                remove_empty_parents(path.parent(), &self.root);
                info!(entry_key = %key, "removed container entry");
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(RemoveError::EntryNotFound(key.clone()))
            }
            Err(error) => Err(RemoveError::Other(error.into())),
        }
    }

    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError> {
        let span = container_operation_span!(self.logger, "rename");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "renaming container entry");
        let from_path = self
            .entry_path(from)
            .map_err(RenameError::InvalidEntryKey)?;
        let to_path = self.entry_path(to).map_err(RenameError::InvalidEntryKey)?;
        if self.is_linked_from(from).map_err(RenameError::Other)? {
            return Err(RenameError::EntryIsLinked(from.clone()));
        }
        if self.is_linked_from(to).map_err(RenameError::Other)? {
            return Err(RenameError::EntryIsLinked(to.clone()));
        }
        if !from_path.is_file() {
            return Err(RenameError::EntryNotFound(from.clone()));
        }
        if fs::symlink_metadata(&to_path).is_ok() {
            return Err(RenameError::EntryExists(to.clone()));
        }
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).map_err(|error| RenameError::Other(error.into()))?;
        }
        fs::rename(&from_path, &to_path).map_err(|error| RenameError::Other(error.into()))?;
        remove_empty_parents(from_path.parent(), &self.root);
        info!(from = %from, to = %to, "renamed container entry");
        Ok(())
    }

    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError> {
        let span = container_operation_span!(self.logger, "copy");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "copying container entry");
        let from_path = self.entry_path(from).map_err(CopyError::InvalidEntryKey)?;
        let to_path = self.entry_path(to).map_err(CopyError::InvalidEntryKey)?;
        if !from_path.is_file() {
            return Err(CopyError::EntryNotFound(from.clone()));
        }
        if self.is_linked_from(to).map_err(CopyError::Other)?
            || fs::symlink_metadata(&to_path).is_ok()
        {
            return Err(CopyError::EntryExists(to.clone()));
        }
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).map_err(|error| CopyError::Other(error.into()))?;
        }
        fs::copy(&from_path, &to_path).map_err(|error| CopyError::Other(error.into()))?;
        info!(from = %from, to = %to, "copied container entry");
        Ok(())
    }

    fn link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError> {
        let span = container_operation_span!(self.logger, "link_from");
        let _entered = span.enter();
        debug!(target_key = %target_key, linker_key = %linker_key, "registering incoming link");
        let target_path = self
            .entry_path(target_key)
            .map_err(LinkFromError::InvalidEntryKey)?;
        validate_key(linker_key).map_err(LinkFromError::InvalidEntryKey)?;
        if !target_path.is_file() {
            return Err(LinkFromError::TargetNotFound(target_key.clone()));
        }

        let mut links = self.links().map_err(LinkFromError::Other)?;
        let incoming = links.links.entry(target_key.clone()).or_default();
        if incoming
            .iter()
            .any(|link| link.linker_uid == linker_uid && link.linker_key == *linker_key)
        {
            return Err(LinkFromError::AlreadyLinked);
        }
        incoming.push(IncomingLink {
            linker_key: linker_key.clone(),
            linker_uid: linker_uid.to_owned(),
        });
        self.save_links(&mut links).map_err(LinkFromError::Other)?;
        info!(
            target_key = %target_key,
            linker_uid,
            linker_key = %linker_key,
            "registered incoming link"
        );
        Ok(())
    }

    fn unlink(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError> {
        let span = container_operation_span!(self.logger, "unlink");
        let _entered = span.enter();
        debug!(target_key = %target_key, linker_key = %linker_key, "removing incoming link");
        validate_key(target_key).map_err(UnlinkError::InvalidEntryKey)?;
        validate_key(linker_key).map_err(UnlinkError::InvalidEntryKey)?;
        let mut links = self.links().map_err(UnlinkError::Other)?;
        let Some(incoming) = links.links.get_mut(target_key) else {
            return Err(UnlinkError::LinkNotFound);
        };
        let Some(position) = incoming.iter().position(|existing| {
            existing.linker_uid == linker_uid && existing.linker_key == *linker_key
        }) else {
            return Err(UnlinkError::LinkMismatch);
        };
        incoming.remove(position);
        if incoming.is_empty() {
            links.links.remove(target_key);
        }
        self.save_links(&mut links).map_err(UnlinkError::Other)?;
        info!(
            target_key = %target_key,
            linker_uid,
            linker_key = %linker_key,
            "removed incoming link"
        );
        Ok(())
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        debug!(entry_key = %key, "getting link information");
        validate_key(key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        let links = self.links()?;
        Ok(match links.links.get(key) {
            Some(links) => {
                let mut linkers = links
                    .iter()
                    .map(|link| super::LinkSource {
                        linker_key: link.linker_key.clone(),
                        linker_uid: link.linker_uid.clone(),
                    })
                    .collect::<Vec<_>>();
                linkers.sort_by(|left, right| {
                    left.linker_uid
                        .cmp(&right.linker_uid)
                        .then_with(|| left.linker_key.cmp(&right.linker_key))
                });
                LinkInfo::LinkFrom { linkers }
            }
            None => LinkInfo::None,
        })
    }

    fn has_link_from(
        &self,
        target_key: &EntryKey,
        linker_uid: &str,
        linker_key: &EntryKey,
    ) -> Result<bool> {
        validate_key(target_key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        Ok(self.links()?.links.get(target_key).is_some_and(|links| {
            links
                .iter()
                .any(|link| link.linker_uid == linker_uid && link.linker_key == *linker_key)
        }))
    }
}

impl Drop for LocalContainerWriteGuard {
    fn drop(&mut self) {
        trace!(
            logger = %self.logger.name(),
            container_uid = %self.logger.uid(),
            container_kind = self.logger.kind(),
            operation = "writer",
            path = %self.root.join(CONTROL_DIR).join(LOCK_FILE).display(),
            "releasing file lock"
        );
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IncomingLinksMetadata {
    version: u32,
    count: u64,
    links: BTreeMap<EntryKey, Vec<IncomingLink>>,
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
struct IncomingLink {
    linker_key: EntryKey,
    linker_uid: String,
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

fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))
}

fn read_incoming_metadata(path: &Path) -> Result<IncomingLinksMetadata> {
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

fn write_json_to(file: File, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn write_entry_atomic(root: &Path, path: &Path, data: &[u8], replace: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary_directory = root.join(CONTROL_DIR).join("tmp");
    fs::create_dir_all(&temporary_directory)?;
    let temporary = temporary_directory.join(Uuid::new_v4().to_string());
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        if replace {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
            fs::remove_file(&temporary)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result: Result<()> = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        write_json_to(file, value)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist {}", path.display()))
}

fn list_entries(root: &Path, directory: &Path, entries: &mut Vec<EntryKey>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
    {
        let entry = entry?;
        if directory == root && entry.file_name() == CONTROL_DIR {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            list_entries(root, &entry.path(), entries)?;
        } else if file_type.is_file() {
            entries.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("entry is below container root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(())
}

fn remove_empty_parents(mut directory: Option<&Path>, root: &Path) {
    while let Some(path) = directory {
        if path == root || !path.starts_with(root) {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => directory = path.parent(),
            Err(_) => break,
        }
    }
}
