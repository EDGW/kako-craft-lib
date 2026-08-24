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
    local: LocalContainer,
    logger: ContainerLogger,
}

impl LinkContainer {
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

    pub fn with_logical_name(
        path: impl Into<PathBuf>,
        logical_name: impl Into<String>,
    ) -> Result<Self> {
        let local = LocalContainer::open_as(path.into(), logical_name.into(), "link")?;
        Self::from_local(local)
    }

    /// Opens an existing link container from already parsed common metadata.
    pub fn from_metadata(path: impl Into<PathBuf>, metadata: ContainerMetadata) -> Result<Self> {
        let local = LocalContainer::from_metadata_as(path.into(), metadata, "link")?;
        Self::from_local(local)
    }

    pub fn path(&self) -> &Path {
        let span = container_operation_span!(self.logger, "path");
        let _entered = span.enter();
        let path = self.local.path_ref();
        trace!(path = %path.display(), "getting container path");
        path
    }

    /// Lists only ordinary local entries, excluding outgoing links.
    pub fn local_list(&self) -> Result<Vec<EntryKey>> {
        self.local.list()
    }

    pub fn local_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        self.local.list_info()
    }

    /// Reads an ordinary local entry without following an outgoing link.
    pub fn local_read(&self, key: &EntryKey) -> Result<Buffer> {
        self.guarded()?.local_read_verified(key)
    }

    /// Resolves the path of an ordinary local entry.
    pub fn local_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.guarded()?.local_filepath_verified(key)
    }

    /// Lists outgoing-link keys only.
    pub fn link_list(&self) -> Result<Vec<EntryKey>> {
        Ok(self.outgoing_metadata()?.links.into_keys().collect())
    }

    pub fn link_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        Ok(self
            .list_info()?
            .into_iter()
            .filter(|entry| matches!(entry.link, LinkInfo::LinkTo { .. }))
            .collect())
    }

    pub fn prefer_relative(&self) -> Result<bool> {
        Ok(self.outgoing_metadata()?.prefer_relative)
    }

    fn from_local(local: LocalContainer) -> Result<Self> {
        let logger = ContainerLogger::new("link", local.uid()?);
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

    fn outgoing_metadata_path(&self) -> PathBuf {
        self.local.path_ref().join(CONTROL_DIR).join(METADATA_FILE)
    }

    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        self.guarded()?.outgoing_metadata()
    }

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
        "link".to_owned()
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
