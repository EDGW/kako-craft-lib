//! Typed errors returned by container storage, validation, linking, and repair.

use std::path::PathBuf;

use thiserror::Error;

use super::{CheckRepairAction, EntryKey};

#[derive(Debug, Error)]
/// Failure to read or migrate a supported link-metadata format.
pub enum LinkMetadataMigrationError {
    /// The file uses a version which requires an explicit migration.
    #[error("link metadata migration is required for version {version} in {path}")]
    Required {
        /// Path of the metadata file requiring migration.
        path: PathBuf,
        /// Format version found in the metadata file.
        version: u32,
    },

    /// Migration was attempted but could not be completed safely.
    #[error("failed to migrate link metadata in {path}: {reason}")]
    Failed {
        /// Path of the metadata file that could not be migrated.
        path: PathBuf,
        /// Human-readable reason why migration was aborted.
        reason: String,
    },
}

#[derive(Debug, Error)]
/// Failure to start or complete a multi-container validation run.
pub enum LinkValidationRunError {
    /// The current container was also supplied as a corresponding container.
    #[error("cannot validate a container against itself: {0}")]
    SelfValidation(String),

    /// Two distinct paths supplied for validation resolve to the same UID.
    #[error("the same container UID was supplied through multiple paths: {uid}")]
    DuplicateContainerUid {
        /// Duplicate container UID resolved from the supplied paths.
        uid: String,
    },

    /// An underlying metadata, path, or I/O operation failed.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A structured failure while applying an explicitly selected check action.
///
/// Keeping these failures typed lets callers distinguish stale/invalid issue
/// input from an I/O failure raised by the concrete repair operation.
#[derive(Debug, Error)]
pub enum CheckActionError {
    /// The issue lacks metadata required by the selected action.
    #[error("check issue {issue_id} has no {field}")]
    MissingContext {
        /// Stable identifier of the incomplete check issue.
        issue_id: String,
        /// Name of the missing issue field.
        field: &'static str,
    },

    /// The selected action is not offered for the specified issue.
    #[error("action {action:?} is not valid for check issue {issue_id}")]
    InvalidAction {
        /// Stable identifier of the check issue.
        issue_id: String,
        /// Action rejected for that issue.
        action: CheckRepairAction,
    },

    /// More than one supplied container has the UID required by an issue.
    #[error("corresponding container UID is ambiguous: {0}")]
    AmbiguousContainerUid(String),

    /// A corresponding path opened a container with the wrong UID.
    #[error("corresponding container UID mismatch at {path}: expected {expected}, found {actual}")]
    ContainerUidMismatch {
        /// Path used to open the corresponding container.
        path: PathBuf,
        /// UID recorded by the check issue.
        expected: String,
        /// UID read from the opened container.
        actual: String,
    },
}

#[derive(Debug, Error)]
/// Failure to acquire a container write guard.
pub enum WriterError {
    /// Another write guard currently owns the non-blocking container lock.
    #[error("container is locked by another writer")]
    ContainerLocked,

    /// Opening or locking the container failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to create or replace an ordinary entry.
pub enum WriteError {
    /// The requested entry key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// The key belongs to an outgoing link and cannot hold local data.
    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    /// Persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to add a new ordinary entry.
pub enum AddError {
    /// The requested entry key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// An ordinary entry or protected link record already owns the key.
    #[error("entry already exists: {0}")]
    EntryExists(EntryKey),

    /// The key belongs to an outgoing link and cannot hold local data.
    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    /// Persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to remove an ordinary entry.
pub enum RemoveError {
    /// The requested entry key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// No ordinary entry exists at the requested key.
    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    /// At least one incoming or outgoing link record protects the entry.
    #[error("entry is linked and must be unlinked before removal: {0}")]
    EntryIsLinked(EntryKey),

    /// Persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to rename an ordinary entry.
pub enum RenameError {
    /// A source or destination key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// The source ordinary entry does not exist.
    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    /// The destination key is already occupied.
    #[error("destination entry already exists: {0}")]
    EntryExists(EntryKey),

    /// An incoming or outgoing link record protects the source or destination.
    #[error("entry is linked and must be unlinked before rename: {0}")]
    EntryIsLinked(EntryKey),

    /// Persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to copy an ordinary entry.
pub enum CopyError {
    /// A source or destination key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// The source ordinary entry does not exist.
    #[error("entry does not exist: {0}")]
    EntryNotFound(EntryKey),

    /// The destination key is already occupied.
    #[error("destination entry already exists: {0}")]
    EntryExists(EntryKey),

    /// The source key identifies an outgoing link rather than local data.
    #[error("entry is an outgoing link: {0}")]
    EntryIsLink(EntryKey),

    /// Persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to register a reciprocal incoming-link record.
pub enum LinkFromError {
    /// The exact linker UID and key are already registered.
    #[error("the link is already registered")]
    AlreadyLinked,

    /// The requested target or linker key conflicts with existing link state.
    #[error("the target key is already linked from another entry")]
    LinkConflict,

    /// The target ordinary entry does not exist.
    #[error("target entry does not exist: {0}")]
    TargetNotFound(EntryKey),

    /// The target or linker key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// Metadata persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to remove a reciprocal incoming-link record.
pub enum UnlinkError {
    /// No incoming record exists for the target key.
    #[error("the link is not registered")]
    LinkNotFound,

    /// Incoming records exist, but none match the supplied linker UID and key.
    #[error("the registered link does not match the supplied link")]
    LinkMismatch,

    /// The target or linker key is unsafe or malformed.
    #[error("invalid entry key: {0}")]
    InvalidEntryKey(EntryKey),

    /// Metadata persistence failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to create or copy an outgoing link.
pub enum LinkToError {
    /// The exact outgoing relationship is already registered.
    #[error("the outgoing link is already registered")]
    AlreadyLinked,
    /// The linker key is registered for a different target.
    #[error("the key has a different outgoing link")]
    LinkConflict,
    /// An ordinary local entry already occupies the linker key.
    #[error("a local entry already exists at: {0}")]
    LocalEntryExists(EntryKey),
    /// The requested ordinary target entry does not exist.
    #[error("target entry does not exist: {0}")]
    TargetNotFound(EntryKey),
    /// The reciprocal incoming-record operation failed.
    #[error(transparent)]
    Target(#[from] LinkFromError),
    /// A required container write lock could not be acquired.
    #[error(transparent)]
    Writer(#[from] WriterError),
    /// Existing link state is broken or temporarily unavailable.
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    /// Some transaction steps completed and rollback was incomplete.
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    /// Path, metadata, or filesystem work failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
/// Failure to remove an outgoing link and its reciprocal record.
pub enum UnlinkToError {
    /// No outgoing record exists for the requested linker key.
    #[error("the outgoing link is not registered")]
    LinkNotFound,
    /// The opened corresponding container does not match recorded identity.
    #[error("container does not match the recorded link")]
    ContainerMismatch,
    /// Removing the reciprocal incoming record failed.
    #[error(transparent)]
    Target(#[from] UnlinkError),
    /// A required container write lock could not be acquired.
    #[error(transparent)]
    Writer(#[from] WriterError),
    /// Existing link state is broken or temporarily unavailable.
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    /// Some transaction steps completed and rollback was incomplete.
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    /// Path, metadata, or filesystem work failed for another reason.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Describes a multi-container mutation whose rollback could not fully restore state.
#[derive(Debug, Error)]
#[error(
    "partial commit during {operation}: {cause}; completed steps: {completed_steps:?}; rollback errors: {rollback_errors:?}"
)]
pub struct LinkPartialCommitError {
    /// Stable name of the mutation being performed.
    pub operation: &'static str,
    /// Error that interrupted the forward transaction.
    pub cause: String,
    /// Human-readable steps known to have completed before the failure.
    pub completed_steps: Vec<String>,
    /// Errors raised while attempting to roll back completed steps.
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Error)]
/// Classifies outgoing-link access as persistently broken or temporarily unavailable.
pub enum LinkAccessError {
    /// Metadata, identity, target, reciprocal record, or symlink is inconsistent.
    #[error(transparent)]
    Broken(#[from] BrokenLinkError),
    /// A required corresponding container cannot currently be locked.
    #[error(transparent)]
    Unavailable(#[from] LinkUnavailableError),
}

impl LinkAccessError {
    /// Erases the access classification while preserving the concrete source error.
    ///
    /// # Returns
    ///
    /// An [`anyhow::Error`] containing the original [`BrokenLinkError`] or
    /// [`LinkUnavailableError`].
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Broken(error) => error.into(),
            Self::Unavailable(error) => error.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("broken outgoing link '{key}': {reason}")]
/// A persistent inconsistency found while validating an outgoing link.
pub struct BrokenLinkError {
    /// Linker key whose outgoing relationship is broken.
    pub key: EntryKey,
    /// Precise validation failure that made the link unsafe to access.
    pub reason: String,
}

impl BrokenLinkError {
    /// Constructs a persistent outgoing-link validation failure.
    ///
    /// # Arguments
    ///
    /// * `key` - Linker entry key whose relationship failed validation.
    /// * `reason` - Precise description of the inconsistent metadata or filesystem state.
    ///
    /// # Returns
    ///
    /// A new error owning a clone of `key` and the supplied reason.
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
/// A validly identified target container which cannot currently be write-locked.
pub struct LinkUnavailableError {
    /// Linker key whose target cannot currently be accessed.
    pub key: EntryKey,
    /// UID recorded for the target container.
    pub container_uid: String,
    /// Resolved path used to locate the target container.
    pub container_path: PathBuf,
    /// Underlying temporary failure, usually lock contention.
    pub reason: String,
}
