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

pub use link::{LinkContainer, LinkToError, UnlinkToError};
pub use local::LocalContainer;

pub const CONTAINER_FORMAT_VERSION: u32 = 1;
pub const CONTROL_DIR: &str = ".kcl";
pub const CONTAINER_METADATA_FILE: &str = "container.json";

pub type Buffer = Vec<u8>;
pub type EntryKey = String;

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
    /// Parses metadata directly from a `container.json` file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
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

    fn list(&self) -> Result<Vec<EntryKey>>;
    fn filepath(&self, key: &EntryKey) -> Result<PathBuf>;
    fn read(&self, key: &EntryKey) -> Result<Buffer>;
    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError>;
}

pub trait ContainerWriteGuard {
    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError>;

    fn link_from(
        &mut self,
        linker: &mut dyn Container,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError>;

    fn unlink(
        &mut self,
        linker: &mut dyn Container,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError>;

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo>;

    fn validate_link(
        &self,
        key: &EntryKey,
        another: &dyn Container,
    ) -> std::result::Result<(), LinkValidationError> {
        let info = self.link_info(key)?;

        // A regular, unlinked entry is valid regardless of `another`.
        let (another_key, expected_other_kind) = match info {
            LinkInfo::None => return Ok(()),
            LinkInfo::LinkTo {
                target_key,
                container_uid,
            } => {
                if container_uid != another.uid()? {
                    return Err(LinkValidationError::ContainerMismatch);
                }
                (target_key, LinkKind::From)
            }
            LinkInfo::LinkFrom {
                linker_key,
                linker_uid,
            } => {
                if linker_uid != another.uid()? {
                    return Err(LinkValidationError::ContainerMismatch);
                }
                (linker_key, LinkKind::To)
            }
        };

        let other_info = another.writer()?.link_info(&another_key)?;
        let matches = match (expected_other_kind, other_info) {
            (LinkKind::From, LinkInfo::LinkFrom { linker_key, .. })
            | (
                LinkKind::To,
                LinkInfo::LinkTo {
                    target_key: linker_key,
                    ..
                },
            ) => linker_key.as_str() == key.as_str(),
            _ => false,
        };

        if matches {
            Ok(())
        } else {
            Err(LinkValidationError::LinkMismatch)
        }
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

enum LinkKind {
    To,
    From,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkInfo {
    LinkTo {
        target_key: EntryKey,
        container_uid: String,
    },
    LinkFrom {
        linker_key: EntryKey,
        linker_uid: String,
    },
    None,
}
