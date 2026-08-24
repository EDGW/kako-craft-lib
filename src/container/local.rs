use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTAINER_FORMAT_VERSION, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, CopyError, EntryKey, LinkFromError, LinkInfo, RemoveError, RenameError,
    UnlinkError, WriteError, WriterError, metadata_path,
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
        debug!(path = %self.path.display(), "getting container path");
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
        debug!("getting container metadata");
        self.metadata.clone()
    }

    fn uid(&self) -> Result<String> {
        let span = container_operation_span!(self.logger, "uid");
        let _entered = span.enter();
        debug!("getting container UID");
        Ok(self.metadata.uid.clone())
    }

    fn logical_name(&self) -> String {
        let span = container_operation_span!(self.logger, "logical_name");
        let _entered = span.enter();
        debug!("getting container logical name");
        self.metadata.logical_name.clone()
    }

    fn kind(&self) -> String {
        let span = container_operation_span!(self.logger, "kind");
        let _entered = span.enter();
        debug!("getting container kind");
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
                trace!(path = %self.lock_path().display(), "file lock is held by another writer");
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
        let links: IncomingLinksMetadata = read_json(&self.links_path)?;
        ensure_version(links.version, &self.links_path)?;
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

    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        let span = container_operation_span!(self.logger, "add");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "adding container entry");
        let path = self.entry_path(key).map_err(AddError::InvalidEntryKey)?;
        if self.is_linked_from(key).map_err(AddError::Other)? {
            return Err(AddError::EntryExists(key.clone()));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| AddError::Other(error.into()))?;
        }
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(AddError::EntryExists(key.clone()));
            }
            Err(error) => return Err(AddError::Other(error.into())),
        };
        if let Err(error) = file.write_all(&data) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(AddError::Other(error.into()));
        }
        Ok(())
    }

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError> {
        let span = container_operation_span!(self.logger, "write");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "writing container entry");
        let path = self.entry_path(key).map_err(WriteError::InvalidEntryKey)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| WriteError::Other(error.into()))?;
        }
        fs::write(&path, data).map_err(|error| WriteError::Other(error.into()))
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
        fs::copy(&from_path, &to_path)
            .map(|_| ())
            .map_err(|error| CopyError::Other(error.into()))
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
        self.save_links(&mut links).map_err(LinkFromError::Other)
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
        self.save_links(&mut links).map_err(UnlinkError::Other)
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        debug!(entry_key = %key, "getting link information");
        validate_key(key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        let links = self.links()?;
        Ok(match links.links.get(key) {
            Some(links) => LinkInfo::LinkFrom {
                linkers: links
                    .iter()
                    .map(|link| super::LinkSource {
                        linker_key: link.linker_key.clone(),
                        linker_uid: link.linker_uid.clone(),
                    })
                    .collect(),
            },
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
            version: CONTAINER_FORMAT_VERSION,
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

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json_to(file: File, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
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

fn ensure_version(version: u32, path: &Path) -> Result<()> {
    if version == CONTAINER_FORMAT_VERSION {
        Ok(())
    } else {
        Err(anyhow!(
            "unsupported format version {version} in {}",
            path.display()
        ))
    }
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
