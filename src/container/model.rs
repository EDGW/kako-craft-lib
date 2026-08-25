//! Public container metadata, link snapshots, validation reports, and check models.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Current format version for `.kcl/container.json`.
pub const CONTAINER_FORMAT_VERSION: u32 = 1;
/// Current format version for a link container's outgoing-link metadata.
pub const OUTGOING_LINKS_FORMAT_VERSION: u32 = 2;
/// Current format version for reciprocal incoming-link metadata.
pub const INCOMING_LINKS_FORMAT_VERSION: u32 = 2;
/// Reserved directory containing container metadata, locks, and temporary files.
pub const CONTROL_DIR: &str = ".kcl";
/// Filename of the metadata document shared by all container kinds.
pub const CONTAINER_METADATA_FILE: &str = "container.json";

/// Raw bytes stored in or read from an ordinary container entry.
pub type Buffer = Vec<u8>;
/// Slash-separated key identifying an entry relative to a container root.
pub type EntryKey = String;

/// One reciprocal record stored by the container that owns the target entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingLinkRecord {
    /// Ordinary entry key being targeted in the record-owning container.
    pub target_key: EntryKey,
    /// Stable UID of the container that owns the outgoing link.
    pub linker_uid: String,
    /// Entry key of the outgoing link in the linker container.
    pub linker_key: EntryKey,
}

/// One outgoing-link record stored by a link container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingLinkRecord {
    /// Entry key occupied by the outgoing link in the linker container.
    pub linker_key: EntryKey,
    /// Stable UID expected from the target container.
    pub target_container_uid: String,
    /// Absolute path or linker-root-relative path used to locate the target container.
    pub target_container_path: PathBuf,
    /// Ordinary entry key targeted in the target container.
    pub target_key: EntryKey,
}

/// Unvalidated incoming and outgoing records captured under a container write lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerLinkSnapshot {
    /// Stable UID of the container that produced the snapshot.
    pub container_uid: String,
    /// Filesystem root of the container that produced the snapshot.
    pub container_path: PathBuf,
    /// All reciprocal incoming records stored by the container.
    pub incoming: Vec<IncomingLinkRecord>,
    /// All outgoing records stored by the container.
    pub outgoing: Vec<OutgoingLinkRecord>,
}

/// A fully validated outgoing/incoming record pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMatch {
    /// Stable UID of the outgoing-link owner.
    pub linker_container_uid: String,
    /// Outgoing-link entry key in the linker container.
    pub linker_key: EntryKey,
    /// Stable UID of the ordinary-entry owner.
    pub target_container_uid: String,
    /// Ordinary target entry key in the target container.
    pub target_key: EntryKey,
}

/// Machine-readable category for a persistent link inconsistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkValidationIssueKind {
    /// A recorded incoming source has no outgoing record in its linker container.
    MissingOutgoingRecord,
    /// A corresponding container has an outgoing record without the expected incoming record.
    UnexpectedOutgoingRecord,
    /// An outgoing record has no exact reciprocal incoming record at its target.
    MissingIncomingRecord,
    /// A corresponding target contains a reciprocal record absent from the current linker.
    UnexpectedIncomingRecord,
    /// A reciprocal record names a different linker container UID.
    LinkerUidMismatch,
    /// The outgoing and incoming sides disagree about target container UID.
    TargetUidMismatch,
    /// The outgoing and incoming sides disagree about linker entry key.
    LinkerKeyMismatch,
    /// The outgoing and incoming sides disagree about target entry key.
    TargetKeyMismatch,
    /// The recorded ordinary target entry is absent or is not an ordinary file.
    TargetEntryMissing,
    /// The recorded target-container path cannot be opened.
    ContainerPathMissing,
    /// The recorded path does not resolve to the supplied corresponding root.
    ContainerPathMismatch,
    /// The recorded path opens a container whose UID differs from the record.
    ContainerPathUidMismatch,
    /// The same reciprocal incoming identity appears more than once.
    DuplicateIncomingRecord,
    /// More than one outgoing candidate represents the same relationship.
    DuplicateOutgoingRecord,
    /// Link metadata exists but cannot be parsed or migrated safely.
    MetadataInvalid,
    /// Outgoing metadata exists but its materialized symbolic link is absent.
    MaterializedSymlinkMissing,
    /// The materialized entry is not the symbolic link calculated from metadata.
    MaterializedSymlinkMismatch,
}

/// Detailed evidence for one persistent link inconsistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkValidationIssue {
    /// Machine-readable inconsistency category.
    pub kind: LinkValidationIssueKind,
    /// Stable UID of the container being inspected.
    pub current_container_uid: String,
    /// Entry key whose relationship is being validated.
    pub current_key: EntryKey,
    /// Stable UID of the corresponding container involved in the issue.
    pub corresponding_container_uid: String,
    /// Filesystem root used to inspect the corresponding container.
    pub corresponding_container_path: PathBuf,
    /// Linker key involved in the issue, when one can be identified.
    pub linker_key: Option<EntryKey>,
    /// Target key involved in the issue, when one can be identified.
    pub target_key: Option<EntryKey>,
    /// Expected identity, path, key, or filesystem state.
    pub expected: Option<String>,
    /// Actual identity, path, key, or filesystem state observed.
    pub actual: Option<String>,
}

/// A corresponding container which could not be inspected temporarily.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkValidationUnavailable {
    /// Expected or discovered UID of the unavailable container.
    pub container_uid: String,
    /// Path through which validation attempted to access the container.
    pub container_path: PathBuf,
    /// Retryable lock or I/O failure reported by the access attempt.
    pub reason: String,
}

/// Complete validation outcome for one current entry and all supplied containers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkValidationReport {
    /// Exact outgoing/incoming record pairs that passed every validation step.
    pub valid: Vec<LinkMatch>,
    /// Persistent inconsistencies that require inspection or repair.
    pub broken: Vec<LinkValidationIssue>,
    /// Corresponding containers that could not currently be inspected.
    pub unavailable: Vec<LinkValidationUnavailable>,
    /// Relevant current-side records whose counterpart UID was not supplied.
    pub ignored_current_records: usize,
    /// Records in supplied containers that are unrelated to the current entry.
    pub ignored_corresponding_records: usize,
    /// Supplied containers with no relationship to the current entry.
    pub ignored_containers: usize,
}

impl LinkValidationReport {
    /// Returns whether the report contains neither broken nor unavailable relationships.
    ///
    /// Ignored records and containers do not make a report invalid.
    ///
    /// # Returns
    ///
    /// `true` when both [`Self::broken`] and [`Self::unavailable`] are empty;
    /// otherwise `false`.
    pub fn is_valid(&self) -> bool {
        self.broken.is_empty() && self.unavailable.is_empty()
    }
}

/// Structured information used to render one listed container entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEntryInfo {
    /// Entry key relative to the container root.
    pub key: EntryKey,
    /// Filesystem path occupied by the ordinary file or symbolic link.
    pub filepath: PathBuf,
    /// File size in bytes, or `None` when metadata cannot be read.
    pub size: Option<u64>,
    /// Incoming, outgoing, or absent link metadata associated with the key.
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
/// Explicit repair operation that may be offered for a check issue.
pub enum CheckRepairAction {
    /// Create the symbolic link required by an existing outgoing record.
    CreateMissingSymlink,
    /// Replace an incorrect symbolic link with the target calculated from metadata.
    ReplaceIncorrectSymlink,
    /// Delete a symbolic link which has no outgoing metadata record.
    DeleteUnrecordedSymlink,
    /// Add the exact reciprocal incoming record required by an outgoing record.
    AddMissingIncomingRecord,
    /// Remove an incoming record proven not to have a matching outgoing side.
    RemoveStaleIncomingRecord,
    /// Recreate an outgoing record and symbolic link from a known incoming record.
    AddMissingOutgoingRecord,
    /// Remove a stale outgoing record together with its symbolic link.
    RemoveStaleOutgoingRecord,
    /// Dangerously remove only local outgoing state when the target cannot be repaired.
    RemoveLocalOutgoingOnly,
    /// Re-run validation for a corresponding container that was temporarily unavailable.
    RetryUnavailable,
    /// Leave the issue unchanged and continue checking other issues.
    Skip,
}

/// Result returned after one selected check action completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckActionResult {
    /// Human-readable description of the concrete mutation or skip performed.
    pub description: String,
}

/// A consistency problem and the explicit actions which are safe to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCheckIssue {
    /// Stable identifier used to distinguish and skip this issue across rechecks.
    pub id: String,
    /// Current-container entry key affected by the issue.
    pub key: EntryKey,
    /// Machine-readable check issue category.
    pub kind: LinkCheckKind,
    /// UID of the corresponding container, when the issue identifies one.
    pub corresponding_container_uid: Option<String>,
    /// Path of the corresponding container, when the issue identifies one.
    pub corresponding_container_path: Option<PathBuf>,
    /// Linker entry key involved in the issue, when known.
    pub linker_key: Option<EntryKey>,
    /// Ordinary target entry key involved in the issue, when known.
    pub target_key: Option<EntryKey>,
    /// Expected metadata or filesystem state, formatted for presentation.
    pub expected: Option<String>,
    /// Actual metadata or filesystem state observed, formatted for presentation.
    pub actual: Option<String>,
    /// Safe or explicitly dangerous actions available for this exact issue.
    pub actions: Vec<CheckRepairAction>,
}

/// Metadata shared by every container implementation and stored in
/// `.kcl/container.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerMetadata {
    /// Container metadata format version.
    pub version: u32,
    /// Stable identity used to validate link relationships independently of paths.
    pub uid: String,
    /// User-facing name associated with the container.
    pub logical_name: String,
    /// Concrete container implementation kind, currently `local` or `link`.
    pub kind: String,
}

/// Link metadata associated with one entry key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkInfo {
    /// The key is an outgoing link to an ordinary entry in another container.
    LinkTo {
        /// Ordinary entry key in the target container.
        target_key: EntryKey,
        /// Stable UID recorded for the target container.
        container_uid: String,
        /// Absolute or linker-root-relative path recorded for the target container.
        container_path: PathBuf,
    },
    /// The key is an ordinary entry referenced by one or more linker sources.
    LinkFrom {
        /// All recorded incoming sources, sorted by UID and linker key when exposed.
        linkers: Vec<LinkSource>,
    },
    /// The key has neither outgoing nor incoming link metadata.
    None,
}

/// Identity of one incoming source which references an ordinary target entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSource {
    /// Entry key of the outgoing link in the source container.
    pub linker_key: EntryKey,
    /// Stable UID of the source container.
    pub linker_uid: String,
}
