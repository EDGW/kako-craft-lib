use std::path::PathBuf;

use thiserror::Error;

use super::{CheckRepairAction, EntryKey};

#[derive(Debug, Error)]
pub enum LinkMetadataMigrationError {
    #[error("link metadata migration is required for version {version} in {path}")]
    Required { path: PathBuf, version: u32 },

    #[error("failed to migrate link metadata in {path}: {reason}")]
    Failed { path: PathBuf, reason: String },
}

#[derive(Debug, Error)]
pub enum LinkValidationRunError {
    #[error("cannot validate a container against itself: {0}")]
    SelfValidation(String),

    #[error("the same container UID was supplied through multiple paths: {uid}")]
    DuplicateContainerUid { uid: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A structured failure while applying an explicitly selected check action.
///
/// Keeping these failures typed lets callers distinguish stale/invalid issue
/// input from an I/O failure raised by the concrete repair operation.
#[derive(Debug, Error)]
pub enum CheckActionError {
    #[error("check issue {issue_id} has no {field}")]
    MissingContext {
        issue_id: String,
        field: &'static str,
    },

    #[error("action {action:?} is not valid for check issue {issue_id}")]
    InvalidAction {
        issue_id: String,
        action: CheckRepairAction,
    },

    #[error("corresponding container UID is ambiguous: {0}")]
    AmbiguousContainerUid(String),

    #[error("corresponding container UID mismatch at {path}: expected {expected}, found {actual}")]
    ContainerUidMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
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
pub enum LinkToError {
    #[error("the outgoing link is already registered")]
    AlreadyLinked,
    #[error("the key has a different outgoing link")]
    LinkConflict,
    #[error("a local entry already exists at: {0}")]
    LocalEntryExists(EntryKey),
    #[error("target entry does not exist: {0}")]
    TargetNotFound(EntryKey),
    #[error(transparent)]
    Target(#[from] LinkFromError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum UnlinkToError {
    #[error("the outgoing link is not registered")]
    LinkNotFound,
    #[error("container does not match the recorded link")]
    ContainerMismatch,
    #[error(transparent)]
    Target(#[from] UnlinkError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
#[error(
    "partial commit during {operation}: {cause}; completed steps: {completed_steps:?}; rollback errors: {rollback_errors:?}"
)]
pub struct LinkPartialCommitError {
    pub operation: &'static str,
    pub cause: String,
    pub completed_steps: Vec<String>,
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Error)]
pub enum LinkAccessError {
    #[error(transparent)]
    Broken(#[from] BrokenLinkError),
    #[error(transparent)]
    Unavailable(#[from] LinkUnavailableError),
}

impl LinkAccessError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Broken(error) => error.into(),
            Self::Unavailable(error) => error.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("broken outgoing link '{key}': {reason}")]
pub struct BrokenLinkError {
    pub key: EntryKey,
    pub reason: String,
}

impl BrokenLinkError {
    pub(crate) fn new(key: &EntryKey, reason: impl Into<String>) -> Self {
        Self {
            key: key.clone(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error(
    "outgoing link '{key}' is temporarily unavailable: target {container_uid} at {container_path}: {reason}"
)]
pub struct LinkUnavailableError {
    pub key: EntryKey,
    pub container_uid: String,
    pub container_path: PathBuf,
    pub reason: String,
}
