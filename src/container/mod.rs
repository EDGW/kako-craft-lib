use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use thiserror::Error;

macro_rules! container_operation_span {
    ($logger:expr, $operation:literal) => {
        tracing::debug_span!(
            "container_operation",
            logger = %$logger.name(),
            container_uid = %$logger.uid(),
            container_kind = $logger.kind(),
            operation = $operation,
        )
    };
}

pub mod link;
pub mod local;

#[cfg(test)]
mod tests;

pub use link::{
    BrokenLinkError, LinkContainer, LinkContainerWriteGuard, LinkToError, UnlinkToError,
};
pub use local::LocalContainer;

pub const CONTAINER_FORMAT_VERSION: u32 = 1;
pub const CONTROL_DIR: &str = ".kcl";
pub const CONTAINER_METADATA_FILE: &str = "container.json";

pub type Buffer = Vec<u8>;
pub type EntryKey = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEntryInfo {
    pub key: EntryKey,
    pub filepath: PathBuf,
    pub size: Option<u64>,
    pub link: LinkInfo,
}

/// Kind of consistency problem found by a link-container check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCheckKind {
    /// An outgoing-link metadata record has no corresponding symlink.
    MissingSymlink,
    /// An outgoing-link metadata record points to a mismatched filesystem entry.
    IncorrectSymlink,
    /// A filesystem symlink exists without an outgoing-link metadata record.
    UnrecordedSymlink,
    /// The recorded target container, UID, or target entry cannot be validated.
    BrokenTarget,
}

/// A repairable link-container consistency problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCheckIssue {
    pub key: EntryKey,
    pub kind: LinkCheckKind,
    pub expected: Option<PathBuf>,
    pub actual: Option<PathBuf>,
}

/// Metadata shared by every container implementation and stored in
/// `.kcl/container.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerMetadata {
    pub version: u32,
    pub uid: String,
    pub logical_name: String,
    pub kind: String,
}

impl ContainerMetadata {
    /// Parses metadata from the JSON bytes of a `container.json` file.
    pub fn from_json(data: impl AsRef<[u8]>) -> Result<Self> {
        let metadata: Self =
            serde_json::from_slice(data.as_ref()).context("failed to parse container.json data")?;
        metadata.validate(Path::new("container.json"))?;
        Ok(metadata)
    }

    /// Parses metadata directly from a `container.json` file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let metadata: Self = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("failed to parse {}", path.display()))?;
        metadata.validate(path)?;
        Ok(metadata)
    }

    /// Loads metadata from a container directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(metadata_path(path.as_ref()))
    }

    /// Opens a concrete container selected by this metadata's `kind`.
    pub fn open(self, path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
        let path = path.into();
        self.validate(&metadata_path(&path))?;
        match self.kind.as_str() {
            "local" => Ok(Box::new(LocalContainer::from_metadata(path, self)?)),
            "link" => Ok(Box::new(LinkContainer::from_metadata(path, self)?)),
            kind => Err(anyhow!("unsupported container kind: {kind}")),
        }
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.version != CONTAINER_FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported format version {} in {}",
                self.version,
                path.display()
            ));
        }
        if self.uid.is_empty() {
            return Err(anyhow!("container UID is empty in {}", path.display()));
        }
        Ok(())
    }
}

/// Reads `.kcl/container.json` and opens the matching concrete container.
pub fn open_container(path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
    let path = path.into();
    ContainerMetadata::load(&path)?.open(path)
}

/// Parses `container.json` bytes and opens the matching concrete container.
pub fn open_container_from_json(
    path: impl Into<PathBuf>,
    data: impl AsRef<[u8]>,
) -> Result<Box<dyn Container>> {
    ContainerMetadata::from_json(data)?.open(path)
}

pub(crate) fn metadata_path(path: &Path) -> PathBuf {
    path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE)
}

pub trait Container {
    fn metadata(&self) -> ContainerMetadata;

    fn uid(&self) -> Result<String> {
        Ok(self.metadata().uid)
    }

    fn logical_name(&self) -> String {
        self.metadata().logical_name
    }

    fn kind(&self) -> String {
        self.metadata().kind
    }

    /// Filesystem root used to reopen and validate this container.
    fn root_path(&self) -> PathBuf;

    fn list(&self) -> Result<Vec<EntryKey>>;

    fn list_info(&self) -> Result<Vec<ContainerEntryInfo>> {
        let keys = self.list()?;
        let guard = self.writer()?;
        keys.into_iter()
            .map(|key| {
                let filepath = self.filepath(&key)?;
                let size = std::fs::metadata(&filepath)
                    .ok()
                    .map(|metadata| metadata.len());
                let link = guard.link_info(&key)?;
                Ok(ContainerEntryInfo {
                    key,
                    filepath,
                    size,
                    link,
                })
            })
            .collect()
    }
    fn filepath(&self, key: &EntryKey) -> Result<PathBuf>;
    fn read(&self, key: &EntryKey) -> Result<Buffer>;
    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError>;
}

pub trait ContainerWriteGuard {
    fn container_uid(&self) -> &str;

    /// Checks link metadata and filesystem links while this write lock is held.
    /// Local containers have no outgoing-link checks and return an empty list.
    fn check(&mut self) -> Result<Vec<LinkCheckIssue>> {
        Ok(Vec::new())
    }

    /// Repairs one issue returned by [`Self::check`], still under this lock.
    fn repair_check(&mut self, _issue: &LinkCheckIssue) -> Result<()> {
        Err(anyhow!(
            "this container does not support link-check repairs"
        ))
    }

    /// Creates a new entry. Unlike [`Self::write`], this never replaces an
    /// existing entry.
    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError>;

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError>;

    /// Removes an ordinary entry. Entries participating in either side of a
    /// link must be unlinked before they can be removed.
    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError>;

    /// Renames an ordinary entry without replacing `to`.
    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError>;

    /// Copies an ordinary entry without replacing `to`.
    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError>;

    fn link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError>;

    fn unlink(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError>;

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo>;

    fn has_link_from(
        &self,
        target_key: &EntryKey,
        linker_uid: &str,
        linker_key: &EntryKey,
    ) -> Result<bool> {
        Ok(matches!(
            self.link_info(target_key)?,
            LinkInfo::LinkFrom { linkers }
                if linkers.iter().any(|recorded| {
                    recorded.linker_uid == linker_uid && recorded.linker_key == *linker_key
                })
        ))
    }

    fn has_link_to(
        &self,
        _linker_key: &EntryKey,
        _target_uid: &str,
        _target_key: &EntryKey,
    ) -> Result<bool> {
        Ok(false)
    }

    fn validate_link(
        &self,
        key: &EntryKey,
        another: &dyn Container,
    ) -> std::result::Result<(), LinkValidationError> {
        let info = self.link_info(key)?;

        // A regular, unlinked entry is valid regardless of `another`.
        match info {
            LinkInfo::None => return Ok(()),
            LinkInfo::LinkTo {
                target_key,
                container_uid,
                ..
            } => {
                if container_uid != another.uid()? {
                    return Err(LinkValidationError::ContainerMismatch);
                }
                if another
                    .writer()?
                    .has_link_from(&target_key, self.container_uid(), key)?
                {
                    return Ok(());
                }
            }
            LinkInfo::LinkFrom { linkers } => {
                let another_uid = another.uid()?;
                let matching = linkers
                    .iter()
                    .find(|linker| linker.linker_uid == another_uid);
                let Some(linker) = matching else {
                    return Err(LinkValidationError::ContainerMismatch);
                };
                if another
                    .writer()?
                    .has_link_to(&linker.linker_key, self.container_uid(), key)?
                {
                    return Ok(());
                }
            }
        }
        Err(LinkValidationError::LinkMismatch)
    }
}

#[derive(Debug, Error)]
pub enum WriterError {
    #[error("container is locked by another writer")]
    ContainerLocked,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum AddError {
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error("entry already exists: {0}")]
    EntryExists(EntryKey),

    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum RemoveError {
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    #[error("entry is linked and must be unlinked before removal: {0}")]
    EntryIsLinked(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum RenameError {
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    #[error("destination entry already exists: {0}")]
    EntryExists(EntryKey),

    #[error("entry is linked and must be unlinked before rename: {0}")]
    EntryIsLinked(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum CopyError {
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    #[error("destination entry already exists: {0}")]
    EntryExists(EntryKey),

    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum LinkFromError {
    #[error("the link is already registered")]
    AlreadyLinked,

    #[error("the target key is already linked from another entry")]
    LinkConflict,

    #[error("target entry does not exist: {0}")]
    TargetNotFound(EntryKey),

    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum UnlinkError {
    #[error("the link is not registered")]
    LinkNotFound,

    #[error("the registered link does not match the supplied link")]
    LinkMismatch,

    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum LinkValidationError {
    #[error("container does not match the recorded link")]
    ContainerMismatch,

    #[error("links recorded by the two containers do not match")]
    LinkMismatch,

    #[error(transparent)]
    Writer(#[from] WriterError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkInfo {
    LinkTo {
        target_key: EntryKey,
        container_uid: String,
        container_path: PathBuf,
    },
    LinkFrom {
        linkers: Vec<LinkSource>,
    },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSource {
    pub linker_key: EntryKey,
    pub linker_uid: String,
}
