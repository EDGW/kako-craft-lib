use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const CONTAINER_FORMAT_VERSION: u32 = 1;
pub const OUTGOING_LINKS_FORMAT_VERSION: u32 = 2;
pub const INCOMING_LINKS_FORMAT_VERSION: u32 = 2;
pub const CONTROL_DIR: &str = ".kcl";
pub const CONTAINER_METADATA_FILE: &str = "container.json";

pub type Buffer = Vec<u8>;
pub type EntryKey = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingLinkRecord {
    pub target_key: EntryKey,
    pub linker_uid: String,
    pub linker_key: EntryKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingLinkRecord {
    pub linker_key: EntryKey,
    pub target_container_uid: String,
    pub target_container_path: PathBuf,
    pub target_key: EntryKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerLinkSnapshot {
    pub container_uid: String,
    pub container_path: PathBuf,
    pub incoming: Vec<IncomingLinkRecord>,
    pub outgoing: Vec<OutgoingLinkRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    pub linker_container_uid: String,
    pub linker_key: EntryKey,
    pub target_container_uid: String,
    pub target_key: EntryKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkValidationIssueKind {
    MissingOutgoingRecord,
    UnexpectedOutgoingRecord,
    MissingIncomingRecord,
    UnexpectedIncomingRecord,
    LinkerUidMismatch,
    TargetUidMismatch,
    LinkerKeyMismatch,
    TargetKeyMismatch,
    TargetEntryMissing,
    ContainerPathMissing,
    ContainerPathMismatch,
    ContainerPathUidMismatch,
    DuplicateIncomingRecord,
    DuplicateOutgoingRecord,
    MetadataInvalid,
    MaterializedSymlinkMissing,
    MaterializedSymlinkMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkValidationIssue {
    pub kind: LinkValidationIssueKind,
    pub current_container_uid: String,
    pub current_key: EntryKey,
    pub corresponding_container_uid: String,
    pub corresponding_container_path: PathBuf,
    pub linker_key: Option<EntryKey>,
    pub target_key: Option<EntryKey>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkValidationUnavailable {
    pub container_uid: String,
    pub container_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkValidationReport {
    pub valid: Vec<LinkMatch>,
    pub broken: Vec<LinkValidationIssue>,
    pub unavailable: Vec<LinkValidationUnavailable>,
    pub ignored_current_records: usize,
    pub ignored_corresponding_records: usize,
    pub ignored_containers: usize,
}

impl LinkValidationReport {
    pub fn is_valid(&self) -> bool {
        self.broken.is_empty() && self.unavailable.is_empty()
    }
}

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
    /// Reciprocal metadata, target identity, or target entry is inconsistent.
    Validation(LinkValidationIssueKind),
    /// A corresponding container could not be inspected temporarily.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckRepairAction {
    CreateMissingSymlink,
    ReplaceIncorrectSymlink,
    DeleteUnrecordedSymlink,
    AddMissingIncomingRecord,
    RemoveStaleIncomingRecord,
    AddMissingOutgoingRecord,
    RemoveStaleOutgoingRecord,
    RemoveLocalOutgoingOnly,
    RetryUnavailable,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckActionResult {
    pub description: String,
}

/// A consistency problem and the explicit actions which are safe to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCheckIssue {
    pub id: String,
    pub key: EntryKey,
    pub kind: LinkCheckKind,
    pub corresponding_container_uid: Option<String>,
    pub corresponding_container_path: Option<PathBuf>,
    pub linker_key: Option<EntryKey>,
    pub target_key: Option<EntryKey>,
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub actions: Vec<CheckRepairAction>,
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
