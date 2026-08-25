//! Link-container handles and lock-owning link mutation APIs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use tracing::{debug, info, trace, warn};

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTROL_DIR, CheckActionError, CheckActionResult, CheckRepairAction,
    Container, ContainerMetadata, ContainerWriteGuard, CopyError, EntryKey, LinkCheckIssue,
    LinkCheckKind, LinkFromError, LinkInfo, LocalContainer, RemoveError, RenameError, UnlinkError,
    WriteError, WriterError, open_container,
};
pub use super::{
    BrokenLinkError, LinkAccessError, LinkPartialCommitError, LinkToError, LinkUnavailableError,
    UnlinkToError,
};

mod access;
mod filesystem;
mod guard;
mod metadata;

pub use self::guard::LinkContainerWriteGuard;

use self::access::validate_outgoing_link_locked;
use self::filesystem::{
    absolute_path, collect_symlinks, materialize_symlink, recorded_container_path,
    resolved_container_path, symlink_target, write_json_atomic,
};
use self::metadata::{OutgoingLink, OutgoingLinksMetadata, read_metadata};

/// Filename of authoritative outgoing-link metadata below the control directory.
const METADATA_FILE: &str = "outgoing-links.json";

#[cfg(test)]
std::thread_local! {
    static FAIL_OUTGOING_METADATA_WRITE_AFTER: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn fail_outgoing_metadata_write_after(successful_writes: usize) {
    FAIL_OUTGOING_METADATA_WRITE_AFTER.set(Some(successful_writes));
}

#[cfg(test)]
fn should_fail_outgoing_metadata_write() -> bool {
    FAIL_OUTGOING_METADATA_WRITE_AFTER.with(|remaining| match remaining.get() {
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

/// A local directory which can contain both ordinary files and outgoing links.
///
/// Ordinary storage and locking are provided by [`LocalContainer`]. Outgoing
/// link metadata in `.kcl/outgoing-links.json` is authoritative; filesystem
/// symlinks are only its materialized representation.
pub struct LinkContainer {
    /// Local-storage implementation providing ordinary entries and the shared writer lock.
    local: LocalContainer,
    /// Link-kind tracing identity sharing the persistent UID of `local`.
    logger: ContainerLogger,
}

impl LinkContainer {
    /// Opens a link container, creating it with relative-path preference when absent.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem directory to open or initialize as a link
    ///   container.
    ///
    /// # Returns
    ///
    /// A link-container handle whose new logical name is derived from the
    /// directory name.
    ///
    /// # Errors
    ///
    /// Returns an error if directories or metadata cannot be created/read, or
    /// existing metadata describes a different container kind.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let logical_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("link")
            .to_owned();
        let local = LocalContainer::open_as(path, logical_name, "link")?;
        Self::from_local(local)
    }

    /// Opens or creates a link container with a caller-selected logical name.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem directory to open or initialize.
    /// * `logical_name` - Name persisted only when new common metadata is
    ///   created.
    ///
    /// # Returns
    ///
    /// A link-container handle for `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if initialization or metadata loading fails, or an
    /// existing container has a different kind.
    pub fn with_logical_name(
        path: impl Into<PathBuf>,
        logical_name: impl Into<String>,
    ) -> Result<Self> {
        let local = LocalContainer::open_as(path.into(), logical_name.into(), "link")?;
        Self::from_local(local)
    }

    /// Opens an existing link container from already parsed common metadata.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem root represented by `metadata`.
    /// * `metadata` - Previously parsed common metadata expected to have kind
    ///   `link`.
    ///
    /// # Returns
    ///
    /// A link-container handle without reparsing `container.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata is invalid, has the wrong kind, or does
    /// not describe a usable container at `path`.
    pub fn from_metadata(path: impl Into<PathBuf>, metadata: ContainerMetadata) -> Result<Self> {
        let local = LocalContainer::from_metadata_as(path.into(), metadata, "link")?;
        Self::from_local(local)
    }

    /// Opens or initializes link-capable storage for another concrete kind.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem root to create or open.
    /// * `logical_name` - Initial logical name used only for new metadata.
    /// * `kind` - Concrete kind whose storage semantics include outgoing links.
    ///
    /// # Returns
    ///
    /// A link-capable handle retaining `kind` in common metadata and logging.
    ///
    /// # Errors
    ///
    /// Returns an error when local storage initialization or kind validation fails.
    pub(crate) fn open_as(path: PathBuf, logical_name: String, kind: &'static str) -> Result<Self> {
        Self::from_local(LocalContainer::open_as(path, logical_name, kind)?)
    }

    /// Wraps parsed common metadata for another link-capable concrete kind.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem root represented by `metadata`.
    /// * `metadata` - Validated common metadata expected to declare `kind`.
    /// * `kind` - Link-capable concrete kind required by the caller.
    ///
    /// # Returns
    ///
    /// A link-capable handle without reparsing common metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata or filesystem state does not match `kind`.
    pub(crate) fn from_metadata_as(
        path: PathBuf,
        metadata: ContainerMetadata,
        kind: &'static str,
    ) -> Result<Self> {
        Self::from_local(LocalContainer::from_metadata_as(path, metadata, kind)?)
    }

    /// Returns the filesystem root of this link container.
    ///
    /// # Returns
    ///
    /// A borrowed path valid for the lifetime of this handle.
    pub fn path(&self) -> &Path {
        let span = container_operation_span!(self.logger, "path");
        let _entered = span.enter();
        let path = self.local.path_ref();
        trace!(path = %path.display(), "getting container path");
        path
    }

    /// Lists only ordinary local entries, excluding outgoing links.
    ///
    /// # Returns
    ///
    /// Ordinary entry keys in deterministic lexical order.
    ///
    /// # Errors
    ///
    /// Returns an error if the container directory cannot be traversed.
    pub fn local_list(&self) -> Result<Vec<EntryKey>> {
        self.local.list()
    }

    /// Lists ordinary local entries with filesystem and incoming-link metadata.
    ///
    /// # Returns
    ///
    /// One structured entry record per key from [`Self::local_list`].
    ///
    /// # Errors
    ///
    /// Returns an error if listing fails, the container lock is unavailable,
    /// or entry/link metadata cannot be read.
    pub fn local_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        self.local.list_info()
    }

    /// Reads an ordinary local entry without following an outgoing link.
    ///
    /// # Arguments
    ///
    /// * `key` - Ordinary local key to read.
    ///
    /// # Returns
    ///
    /// A newly allocated byte buffer containing the local file contents.
    ///
    /// # Errors
    ///
    /// Returns an error if `key` is invalid, identifies an outgoing link, the
    /// lock cannot be acquired, or the file cannot be read.
    pub fn local_read(&self, key: &EntryKey) -> Result<Buffer> {
        self.guarded()?.local_read_verified(key)
    }

    /// Resolves the path of an ordinary local entry.
    ///
    /// # Arguments
    ///
    /// * `key` - Ordinary local key to resolve.
    ///
    /// # Returns
    ///
    /// The local filesystem path beneath this container root.
    ///
    /// # Errors
    ///
    /// Returns an error if `key` is invalid, identifies an outgoing link, or
    /// the lock/metadata cannot be read.
    pub fn local_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.guarded()?.local_filepath_verified(key)
    }

    /// Lists outgoing-link keys only.
    ///
    /// # Returns
    ///
    /// Outgoing-link keys in deterministic lexical order.
    ///
    /// # Errors
    ///
    /// Returns an error when outgoing metadata cannot be read, validated, or
    /// migrated safely.
    pub fn link_list(&self) -> Result<Vec<EntryKey>> {
        Ok(self.outgoing_metadata()?.links.into_keys().collect())
    }

    /// Lists outgoing links with filesystem and raw target metadata.
    ///
    /// # Returns
    ///
    /// Structured entry records for every key from [`Self::link_list`].
    ///
    /// # Errors
    ///
    /// Returns an error if listing fails, the lock is unavailable, or raw
    /// outgoing metadata cannot be read.
    pub fn link_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        Ok(self
            .list_info()?
            .into_iter()
            .filter(|entry| matches!(entry.link, LinkInfo::LinkTo { .. }))
            .collect())
    }

    /// Returns the container-level default path policy for newly created links.
    ///
    /// # Returns
    ///
    /// `true` when new links should prefer relative target-container paths;
    /// `false` when they should use absolute paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the write lock cannot be acquired or outgoing
    /// metadata cannot be read or migrated.
    pub fn prefer_relative(&self) -> Result<bool> {
        Ok(self.outgoing_metadata()?.prefer_relative)
    }

    /// Wraps validated local storage as a link-container handle.
    ///
    /// # Arguments
    ///
    /// * `local` - Local-storage handle already validated with a link-capable concrete kind.
    ///
    /// # Returns
    ///
    /// A link-container handle with tracing context derived from the storage UID.
    ///
    /// # Errors
    ///
    /// Returns an error if the local storage UID cannot be read.
    fn from_local(local: LocalContainer) -> Result<Self> {
        let kind = local.kind();
        let kind = match kind.as_str() {
            "link" => "link",
            "configurable" => "configurable",
            other => return Err(anyhow!("unsupported link-capable container kind: {other}")),
        };
        let logger = ContainerLogger::new(kind, local.uid()?);
        debug!(
            logger = %logger.name(),
            container_uid = %logger.uid(),
            container_kind = logger.kind(),
            operation = "open",
            path = %local.path_ref().display(),
            "opened container"
        );
        Ok(Self { local, logger })
    }

    /// Resolves this link container's outgoing metadata-file path.
    ///
    /// # Returns
    ///
    /// `<root>/.kcl/outgoing-links.json` without accessing the filesystem.
    fn outgoing_metadata_path(&self) -> PathBuf {
        self.local.path_ref().join(CONTROL_DIR).join(METADATA_FILE)
    }

    /// Loads outgoing metadata while holding the shared container writer lock.
    ///
    /// # Returns
    ///
    /// Validated current-version outgoing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the writer cannot be acquired or metadata cannot be read or migrated.
    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        self.guarded()?.outgoing_metadata()
    }

    /// Acquires local storage's writer and assembles the link-aware guard.
    ///
    /// # Returns
    ///
    /// A guard owning the exclusive advisory lock and paths required for outgoing mutations.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::ContainerLocked`] on contention or [`WriterError::Other`] when lock
    /// setup fails.
    fn guarded(&self) -> std::result::Result<LinkContainerWriteGuard, WriterError> {
        Ok(LinkContainerWriteGuard {
            root: self.local.path_ref().to_owned(),
            uid: self.logger.uid().to_owned(),
            metadata_path: self.outgoing_metadata_path(),
            local: self.local.writer()?,
            logger: self.logger.clone(),
        })
    }

    /// Acquires the link container's exclusive writer. All outgoing-link
    /// mutations are methods on the returned guard.
    ///
    /// # Returns
    ///
    /// A concrete [`LinkContainerWriteGuard`] which owns the lock until drop.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::ContainerLocked`] if another writer owns the
    /// lock, or [`WriterError::Other`] when lock setup fails.
    pub fn writer(&self) -> std::result::Result<LinkContainerWriteGuard, WriterError> {
        self.guarded()
    }
}

impl Container for LinkContainer {
    fn root_path(&self) -> PathBuf {
        self.local.path_ref().to_owned()
    }

    fn metadata(&self) -> ContainerMetadata {
        let span = container_operation_span!(self.logger, "metadata");
        let _entered = span.enter();
        trace!("getting container metadata");
        self.local.metadata()
    }

    fn uid(&self) -> Result<String> {
        let span = container_operation_span!(self.logger, "uid");
        let _entered = span.enter();
        trace!("getting container UID");
        Ok(self.logger.uid().to_owned())
    }

    fn logical_name(&self) -> String {
        let span = container_operation_span!(self.logger, "logical_name");
        let _entered = span.enter();
        trace!("getting container logical name");
        self.local.logical_name()
    }

    fn kind(&self) -> String {
        let span = container_operation_span!(self.logger, "kind");
        let _entered = span.enter();
        trace!("getting container kind");
        self.local.kind()
    }

    fn list(&self) -> Result<Vec<EntryKey>> {
        let span = container_operation_span!(self.logger, "list");
        let _entered = span.enter();
        debug!("listing container entries");
        let mut keys: BTreeSet<_> = self.local.list()?.into_iter().collect();
        keys.extend(self.link_list()?);
        debug!(entry_count = keys.len(), "listed container entries");
        Ok(keys.into_iter().collect())
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        let span = container_operation_span!(self.logger, "filepath");
        let _entered = span.enter();
        debug!(entry_key = %key, "resolving entry path");
        self.guarded()?.filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let span = container_operation_span!(self.logger, "read");
        let _entered = span.enter();
        debug!(entry_key = %key, "reading container entry");
        self.guarded()?.read(key)
    }

    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError> {
        let span = container_operation_span!(self.logger, "writer");
        let _entered = span.enter();
        debug!("creating container writer");
        Ok(Box::new(self.guarded()?))
    }
}
