use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use tracing::{debug, info, trace};
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTAINER_FORMAT_VERSION, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, CopyError, EntryKey, LinkFromError, LinkInfo, RemoveError, RenameError,
    UnlinkError, WriteError, WriterError, metadata_path,
};

mod filesystem;
mod guard;
mod metadata;

pub use self::guard::LocalContainerWriteGuard;

use self::filesystem::{
    list_entries, remove_empty_parents, write_entry_atomic, write_json_atomic, write_json_to,
};
pub(crate) use self::metadata::validate_key;
use self::metadata::{IncomingLink, IncomingLinksMetadata, open_lock_file, read_incoming_metadata};

const LINKS_FILE: &str = "links.json";

#[cfg(test)]
std::thread_local! {
    static FAIL_INCOMING_METADATA_WRITE_AFTER: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn fail_incoming_metadata_write_after(successful_writes: usize) {
    FAIL_INCOMING_METADATA_WRITE_AFTER.set(Some(successful_writes));
}

#[cfg(test)]
fn should_fail_incoming_metadata_write() -> bool {
    FAIL_INCOMING_METADATA_WRITE_AFTER.with(|remaining| match remaining.get() {
        Some(0) => {
            remaining.set(None);
            true
        }
        Some(count) => {
            remaining.set(Some(count - 1));
            false
        }
        None => false,
    })
}
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
