use std::collections::{BTreeMap, BTreeSet};
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
    BrokenLinkError, LinkAccessError, LinkContainer, LinkContainerWriteGuard,
    LinkPartialCommitError, LinkToError, LinkUnavailableError, UnlinkToError,
};
pub use local::LocalContainer;

pub const CONTAINER_FORMAT_VERSION: u32 = 1;
pub const OUTGOING_LINKS_FORMAT_VERSION: u32 = 2;
pub const INCOMING_LINKS_FORMAT_VERSION: u32 = 2;
pub const CONTROL_DIR: &str = ".kcl";
pub const CONTAINER_METADATA_FILE: &str = "container.json";

pub type Buffer = Vec<u8>;
pub type EntryKey = String;

#[derive(Debug, Error)]
pub enum LinkMetadataMigrationError {
    #[error("link metadata migration is required for version {version} in {path}")]
    Required { path: PathBuf, version: u32 },

    #[error("failed to migrate link metadata in {path}: {reason}")]
    Failed { path: PathBuf, reason: String },
}

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

#[derive(Debug, Error)]
pub enum LinkValidationRunError {
    #[error("cannot validate a container against itself: {0}")]
    SelfValidation(String),

    #[error("the same container UID was supplied through multiple paths: {uid}")]
    DuplicateContainerUid { uid: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
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
                let filepath = guard.entry_filepath(&key)?;
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

    fn link_snapshot(&self) -> Result<ContainerLinkSnapshot>;

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf>;

    /// Resolves the entry's path without following or validating an outgoing
    /// link. This is used for complete list reports under an existing writer.
    fn entry_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer>;

    /// Checks link metadata and filesystem links while this write lock is held.
    /// Local containers have no outgoing-link checks and return an empty list.
    fn check(&mut self, corresponding: &[&dyn Container]) -> Result<Vec<LinkCheckIssue>> {
        validation_check_issues(self, corresponding)
    }

    /// Applies one explicitly selected action while this writer remains held.
    fn apply_check_action(
        &mut self,
        issue: &LinkCheckIssue,
        action: CheckRepairAction,
        corresponding: &[&dyn Container],
    ) -> Result<CheckActionResult> {
        apply_validation_check_action(self, issue, action, corresponding)
    }

    /// Creates a new entry. Unlike [`Self::write`], this never replaces an
    /// existing entry.
    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError>;

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError>;

    /// Removes an ordinary entry. Entries participating in either side of a
    /// link must be unlinked before they can be removed.
    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError>;

    /// Preflights a batch before removing any entry. Duplicate keys are
    /// removed once; a validation error leaves the whole batch untouched.
    fn remove_many(&mut self, keys: &[EntryKey]) -> std::result::Result<(), RemoveError> {
        let unique = keys.iter().cloned().collect::<BTreeSet<_>>();
        for key in &unique {
            if !matches!(
                self.link_info(key).map_err(RemoveError::Other)?,
                LinkInfo::None
            ) {
                return Err(RemoveError::EntryIsLinked(key.clone()));
            }
            let path = self.filepath(key).map_err(RemoveError::Other)?;
            if !path.is_file() {
                return Err(RemoveError::EntryNotFound(key.clone()));
            }
        }
        for key in unique {
            self.remove(&key)?;
        }
        Ok(())
    }

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

    /// Recreates only a missing outgoing side during a checked repair. The
    /// reciprocal incoming record must already exist and be protected by the
    /// caller's current writer lock.
    fn repair_add_outgoing(
        &mut self,
        _linker_key: &EntryKey,
        _target_container_uid: &str,
        _target_container_path: &Path,
        _target_key: &EntryKey,
    ) -> Result<()> {
        Err(anyhow!("this container cannot create outgoing links"))
    }

    /// Removes only the local outgoing record and symlink during a checked
    /// repair where the reciprocal incoming side is known to be absent.
    fn repair_remove_outgoing(&mut self, _linker_key: &EntryKey) -> Result<()> {
        Err(anyhow!("this container cannot remove outgoing links"))
    }

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

    fn validate_links(
        &self,
        key: &EntryKey,
        corresponding: &[&dyn Container],
    ) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
        let current = self.link_snapshot()?;
        let mut report = validate_link_snapshots(&current, key, corresponding)?;
        let supplied_uids = corresponding
            .iter()
            .map(|container| container.uid())
            .collect::<Result<BTreeSet<_>>>()?;
        let missing_outgoing = current
            .outgoing
            .iter()
            .filter(|record| {
                record.linker_key == *key && !supplied_uids.contains(&record.target_container_uid)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !missing_outgoing.is_empty() {
            let missing_count = missing_outgoing.len();
            let mut recorded = current.clone();
            recorded.outgoing = missing_outgoing;
            merge_validation_report(
                &mut report,
                validate_recorded_link_snapshots(&recorded, key)?,
            );
            report.ignored_current_records =
                report.ignored_current_records.saturating_sub(missing_count);
        }
        sort_validation_report(&mut report);
        log_validation_report(&current.container_uid, key, &report);
        Ok(report)
    }

    /// Validates an outgoing link by reopening the target recorded in its
    /// metadata. Incoming-only entries need explicit corresponding containers
    /// and therefore return an empty report here.
    fn validate_recorded_links(
        &self,
        key: &EntryKey,
    ) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
        let current = self.link_snapshot()?;
        let mut report = validate_recorded_link_snapshots(&current, key)?;
        sort_validation_report(&mut report);
        log_validation_report(&current.container_uid, key, &report);
        Ok(report)
    }
}

fn validation_check_issues<G: ContainerWriteGuard + ?Sized>(
    guard: &G,
    corresponding: &[&dyn Container],
) -> Result<Vec<LinkCheckIssue>> {
    let snapshot = guard.link_snapshot()?;
    let keys = snapshot
        .incoming
        .iter()
        .map(|record| record.target_key.clone())
        .chain(
            snapshot
                .outgoing
                .iter()
                .map(|record| record.linker_key.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut issues = Vec::new();
    for key in keys {
        let report = guard.validate_links(&key, corresponding)?;
        let current_is_outgoing = snapshot
            .outgoing
            .iter()
            .any(|record| record.linker_key == key);
        for issue in report.broken {
            if matches!(
                issue.kind,
                LinkValidationIssueKind::MaterializedSymlinkMissing
                    | LinkValidationIssueKind::MaterializedSymlinkMismatch
            ) {
                continue;
            }
            let corresponding_is_link = corresponding.iter().any(|container| {
                container
                    .uid()
                    .is_ok_and(|uid| uid == issue.corresponding_container_uid)
                    && container.kind() == "link"
            });
            let actions = match issue.kind {
                LinkValidationIssueKind::MissingOutgoingRecord if corresponding_is_link => vec![
                    CheckRepairAction::AddMissingOutgoingRecord,
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::MissingOutgoingRecord => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::MissingIncomingRecord if current_is_outgoing => vec![
                    CheckRepairAction::AddMissingIncomingRecord,
                    CheckRepairAction::RemoveStaleOutgoingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::UnexpectedOutgoingRecord => vec![
                    CheckRepairAction::AddMissingIncomingRecord,
                    CheckRepairAction::RemoveStaleOutgoingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::UnexpectedIncomingRecord => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::TargetUidMismatch
                | LinkValidationIssueKind::TargetKeyMismatch => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::ContainerPathMissing
                | LinkValidationIssueKind::ContainerPathMismatch
                | LinkValidationIssueKind::ContainerPathUidMismatch
                | LinkValidationIssueKind::TargetEntryMissing
                    if current_is_outgoing =>
                {
                    vec![
                        CheckRepairAction::RemoveLocalOutgoingOnly,
                        CheckRepairAction::Skip,
                    ]
                }
                _ => vec![CheckRepairAction::Skip],
            };
            issues.push(LinkCheckIssue {
                id: format!(
                    "validation:{:?}:{}:{}:{}:{}",
                    issue.kind,
                    issue.current_key,
                    issue.corresponding_container_uid,
                    issue.linker_key.as_deref().unwrap_or("-"),
                    issue.target_key.as_deref().unwrap_or("-")
                ),
                key: issue.current_key,
                kind: LinkCheckKind::Validation(issue.kind),
                corresponding_container_uid: Some(issue.corresponding_container_uid),
                corresponding_container_path: Some(issue.corresponding_container_path),
                linker_key: issue.linker_key,
                target_key: issue.target_key,
                expected: issue.expected,
                actual: issue.actual,
                actions,
            });
        }
        for unavailable in report.unavailable {
            issues.push(LinkCheckIssue {
                id: format!(
                    "unavailable:{}:{}:{}",
                    key,
                    unavailable.container_uid,
                    unavailable.container_path.display()
                ),
                key: key.clone(),
                kind: LinkCheckKind::Unavailable,
                corresponding_container_uid: Some(unavailable.container_uid),
                corresponding_container_path: Some(unavailable.container_path),
                linker_key: None,
                target_key: None,
                expected: None,
                actual: Some(unavailable.reason),
                actions: vec![CheckRepairAction::RetryUnavailable, CheckRepairAction::Skip],
            });
        }
    }
    issues.sort_by(|left, right| left.id.cmp(&right.id));
    issues.dedup_by(|left, right| left.id == right.id);
    Ok(issues)
}

fn apply_validation_check_action<G: ContainerWriteGuard + ?Sized>(
    guard: &mut G,
    issue: &LinkCheckIssue,
    action: CheckRepairAction,
    corresponding: &[&dyn Container],
) -> Result<CheckActionResult> {
    if action == CheckRepairAction::Skip {
        return Ok(CheckActionResult {
            description: format!("skipped issue {}", issue.id),
        });
    }
    if issue.kind == LinkCheckKind::Unavailable && action == CheckRepairAction::RetryUnavailable {
        return Ok(CheckActionResult {
            description: format!("retry requested for {}", issue.id),
        });
    }
    let current_uid = guard.container_uid().to_owned();
    let corresponding_uid = issue
        .corresponding_container_uid
        .as_deref()
        .ok_or_else(|| anyhow!("check issue has no corresponding container UID"))?;
    let linker_key = issue
        .linker_key
        .as_ref()
        .ok_or_else(|| anyhow!("check issue has no linker key"))?;
    let target_key = issue
        .target_key
        .as_ref()
        .ok_or_else(|| anyhow!("check issue has no target key"))?;

    match (issue.kind.clone(), action) {
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord),
            CheckRepairAction::AddMissingOutgoingRecord,
        ) => {
            let current = guard.link_snapshot()?;
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.repair_add_outgoing(
                linker_key,
                &current.container_uid,
                &current.container_path,
                &issue.key,
            )?;
            Ok(CheckActionResult {
                description: format!(
                    "created missing outgoing record {}:{} -> {}:{}",
                    corresponding_uid, linker_key, current.container_uid, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            guard.unlink(corresponding_uid, &issue.key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale incoming record {}:{} from {}",
                    corresponding_uid, linker_key, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedOutgoingRecord),
            CheckRepairAction::AddMissingIncomingRecord,
        ) => {
            guard.link_from(corresponding_uid, &issue.key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "added incoming record {}:{} to {}",
                    corresponding_uid, linker_key, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingIncomingRecord),
            CheckRepairAction::AddMissingIncomingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.link_from(&current_uid, target_key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "added reciprocal incoming record {}:{} to {}:{}",
                    current_uid, linker_key, corresponding_uid, target_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingIncomingRecord),
            CheckRepairAction::RemoveStaleOutgoingRecord,
        ) => {
            guard.repair_remove_outgoing(&issue.key)?;
            Ok(CheckActionResult {
                description: format!("removed stale outgoing record and symlink {}", issue.key),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedOutgoingRecord),
            CheckRepairAction::RemoveStaleOutgoingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.repair_remove_outgoing(linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale outgoing record and symlink {}:{}",
                    corresponding_uid, linker_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedIncomingRecord),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.unlink(&current_uid, target_key, &issue.key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale incoming record {}:{} from {}:{}",
                    current_uid, issue.key, corresponding_uid, target_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(
                LinkValidationIssueKind::TargetUidMismatch
                | LinkValidationIssueKind::TargetKeyMismatch,
            ),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            let current = guard.link_snapshot()?;
            if current
                .outgoing
                .iter()
                .any(|record| record.linker_key == issue.key)
            {
                let mut writer = issue_corresponding_writer(issue, corresponding)?;
                writer.unlink(&current_uid, target_key, &issue.key)?;
            } else {
                guard.unlink(corresponding_uid, &issue.key, linker_key)?;
            }
            Ok(CheckActionResult {
                description: format!(
                    "removed mismatched incoming record for {}:{}",
                    corresponding_uid, linker_key
                ),
            })
        }
        _ => Err(anyhow!(
            "action {action:?} is not valid for check issue {}",
            issue.id
        )),
    }
}

fn corresponding_by_uid<'a>(
    corresponding: &'a [&dyn Container],
    uid: &str,
) -> Result<Option<&'a dyn Container>> {
    let mut matched = None;
    for container in corresponding {
        if container.uid()? == uid {
            if matched.is_some() {
                return Err(anyhow!("corresponding container UID is ambiguous: {uid}"));
            }
            matched = Some(*container);
        }
    }
    Ok(matched)
}

fn issue_corresponding_writer(
    issue: &LinkCheckIssue,
    corresponding: &[&dyn Container],
) -> Result<Box<dyn ContainerWriteGuard>> {
    let uid = issue
        .corresponding_container_uid
        .as_deref()
        .ok_or_else(|| anyhow!("check issue has no corresponding container UID"))?;
    match corresponding_by_uid(corresponding, uid)? {
        Some(container) => container.writer().map_err(Into::into),
        None => {
            let path = issue
                .corresponding_container_path
                .as_ref()
                .ok_or_else(|| anyhow!("check issue has no corresponding container path"))?;
            let container = open_container(path)?;
            let actual_uid = container.uid()?;
            if actual_uid != uid {
                return Err(anyhow!(
                    "corresponding container UID mismatch at {}: expected {}, found {}",
                    path.display(),
                    uid,
                    actual_uid
                ));
            }
            container.writer().map_err(Into::into)
        }
    }
}

struct CorrespondingSnapshot {
    uid: String,
    path: PathBuf,
    snapshot: Option<ContainerLinkSnapshot>,
}

fn validate_recorded_link_snapshots(
    current: &ContainerLinkSnapshot,
    key: &EntryKey,
) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
    let outgoing: Vec<_> = current
        .outgoing
        .iter()
        .filter(|record| record.linker_key == *key)
        .collect();
    if outgoing.is_empty() {
        return Ok(LinkValidationReport::default());
    }

    let mut report = LinkValidationReport::default();
    let mut containers = Vec::new();
    for record in outgoing {
        let path = resolved_recorded_path(current, record);
        let container = match open_container(&path) {
            Ok(container) => container,
            Err(error) if is_transient_error(&error) => {
                report.unavailable.push(LinkValidationUnavailable {
                    container_uid: record.target_container_uid.clone(),
                    container_path: path,
                    reason: error.to_string(),
                });
                continue;
            }
            Err(error) => {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::ContainerPathMissing,
                    current,
                    key,
                    &record.target_container_uid,
                    &path,
                    Some(&record.linker_key),
                    Some(&record.target_key),
                    Some(&record.target_container_uid),
                    Some(&error.to_string()),
                );
                continue;
            }
        };
        let actual_uid = container.uid()?;
        if actual_uid != record.target_container_uid {
            push_issue(
                &mut report,
                LinkValidationIssueKind::ContainerPathUidMismatch,
                current,
                key,
                &actual_uid,
                &path,
                Some(&record.linker_key),
                Some(&record.target_key),
                Some(&record.target_container_uid),
                Some(&actual_uid),
            );
            continue;
        }
        containers.push(container);
    }
    if !containers.is_empty() {
        let refs = containers
            .iter()
            .map(|container| container.as_ref())
            .collect::<Vec<_>>();
        merge_validation_report(&mut report, validate_link_snapshots(current, key, &refs)?);
    }
    Ok(report)
}

fn merge_validation_report(into: &mut LinkValidationReport, mut other: LinkValidationReport) {
    into.valid.append(&mut other.valid);
    into.broken.append(&mut other.broken);
    into.unavailable.append(&mut other.unavailable);
    into.ignored_current_records += other.ignored_current_records;
    into.ignored_corresponding_records += other.ignored_corresponding_records;
    into.ignored_containers += other.ignored_containers;
}

fn sort_validation_report(report: &mut LinkValidationReport) {
    report.valid.sort_by(|left, right| {
        (
            &left.linker_container_uid,
            &left.linker_key,
            &left.target_container_uid,
            &left.target_key,
        )
            .cmp(&(
                &right.linker_container_uid,
                &right.linker_key,
                &right.target_container_uid,
                &right.target_key,
            ))
    });
    report.broken.sort_by(|left, right| {
        (
            validation_issue_rank(left.kind),
            &left.corresponding_container_uid,
            &left.current_key,
            &left.linker_key,
            &left.target_key,
        )
            .cmp(&(
                validation_issue_rank(right.kind),
                &right.corresponding_container_uid,
                &right.current_key,
                &right.linker_key,
                &right.target_key,
            ))
    });
    report.unavailable.sort_by(|left, right| {
        (&left.container_uid, &left.container_path)
            .cmp(&(&right.container_uid, &right.container_path))
    });
}

fn validation_issue_rank(kind: LinkValidationIssueKind) -> u8 {
    match kind {
        LinkValidationIssueKind::MissingOutgoingRecord => 0,
        LinkValidationIssueKind::UnexpectedOutgoingRecord => 1,
        LinkValidationIssueKind::MissingIncomingRecord => 2,
        LinkValidationIssueKind::UnexpectedIncomingRecord => 3,
        LinkValidationIssueKind::LinkerUidMismatch => 4,
        LinkValidationIssueKind::TargetUidMismatch => 5,
        LinkValidationIssueKind::LinkerKeyMismatch => 6,
        LinkValidationIssueKind::TargetKeyMismatch => 7,
        LinkValidationIssueKind::TargetEntryMissing => 8,
        LinkValidationIssueKind::ContainerPathMissing => 9,
        LinkValidationIssueKind::ContainerPathMismatch => 10,
        LinkValidationIssueKind::ContainerPathUidMismatch => 11,
        LinkValidationIssueKind::DuplicateIncomingRecord => 12,
        LinkValidationIssueKind::DuplicateOutgoingRecord => 13,
        LinkValidationIssueKind::MetadataInvalid => 14,
        LinkValidationIssueKind::MaterializedSymlinkMissing => 15,
        LinkValidationIssueKind::MaterializedSymlinkMismatch => 16,
    }
}

fn log_validation_report(container_uid: &str, key: &EntryKey, report: &LinkValidationReport) {
    if report.broken.is_empty() && report.unavailable.is_empty() {
        tracing::debug!(
            container_uid,
            entry_key = %key,
            valid = report.valid.len(),
            ignored_current_records = report.ignored_current_records,
            ignored_corresponding_records = report.ignored_corresponding_records,
            ignored_containers = report.ignored_containers,
            "link validation completed"
        );
    } else {
        tracing::warn!(
            container_uid,
            entry_key = %key,
            broken = report.broken.len(),
            unavailable = report.unavailable.len(),
            "link validation found unresolved relationships"
        );
    }
}

fn validate_link_snapshots(
    current: &ContainerLinkSnapshot,
    key: &EntryKey,
    corresponding: &[&dyn Container],
) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
    let current_path = normalized_path(&current.container_path)?;
    let mut report = LinkValidationReport::default();
    let mut seen_uid_paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut snapshots = Vec::new();
    let mut inputs = Vec::new();

    for container in corresponding {
        let uid = container.uid()?;
        let path = normalized_path(&container.root_path())?;
        if uid == current.container_uid {
            return Err(LinkValidationRunError::SelfValidation(uid));
        }
        if let Some(previous) = seen_uid_paths.get(&uid) {
            if previous == &path {
                continue;
            }
            return Err(LinkValidationRunError::DuplicateContainerUid { uid });
        }
        seen_uid_paths.insert(uid.clone(), path.clone());
        inputs.push((*container, uid, path));
    }
    inputs.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.2.cmp(&right.2)));

    for (container, uid, path) in inputs {
        match container.writer() {
            Ok(guard) => match guard.link_snapshot() {
                Ok(snapshot) => snapshots.push(CorrespondingSnapshot {
                    uid,
                    path,
                    snapshot: Some(snapshot),
                }),
                Err(error) => {
                    let referenced = current
                        .incoming
                        .iter()
                        .any(|record| record.target_key == *key && record.linker_uid == uid)
                        || current.outgoing.iter().any(|record| {
                            record.linker_key == *key && record.target_container_uid == uid
                        });
                    if referenced && !is_unavailable_snapshot_error(&error) {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::MetadataInvalid,
                            current,
                            key,
                            &uid,
                            &path,
                            None,
                            None,
                            Some("valid link metadata"),
                            Some(&error.to_string()),
                        );
                    } else {
                        report.unavailable.push(LinkValidationUnavailable {
                            container_uid: uid.clone(),
                            container_path: path.clone(),
                            reason: error.to_string(),
                        });
                    }
                    snapshots.push(CorrespondingSnapshot {
                        uid,
                        path,
                        snapshot: None,
                    });
                }
            },
            Err(error) => {
                report.unavailable.push(LinkValidationUnavailable {
                    container_uid: uid.clone(),
                    container_path: path.clone(),
                    reason: error.to_string(),
                });
                snapshots.push(CorrespondingSnapshot {
                    uid,
                    path,
                    snapshot: None,
                });
            }
        }
    }

    let supplied_uids: BTreeSet<_> = snapshots.iter().map(|item| item.uid.as_str()).collect();
    let current_incoming: Vec<_> = current
        .incoming
        .iter()
        .filter(|record| record.target_key == *key)
        .collect();
    let current_outgoing: Vec<_> = current
        .outgoing
        .iter()
        .filter(|record| record.linker_key == *key)
        .collect();

    report.ignored_current_records += current_incoming
        .iter()
        .filter(|record| !supplied_uids.contains(record.linker_uid.as_str()))
        .count();
    report.ignored_current_records += current_outgoing
        .iter()
        .filter(|record| !supplied_uids.contains(record.target_container_uid.as_str()))
        .count();

    let mut valid_seen = BTreeSet::new();
    for corresponding in snapshots {
        let current_refs_this_uid = current_incoming
            .iter()
            .any(|record| record.linker_uid == corresponding.uid)
            || current_outgoing
                .iter()
                .any(|record| record.target_container_uid == corresponding.uid);

        let Some(other) = corresponding.snapshot.as_ref() else {
            if !current_refs_this_uid {
                // We cannot prove that a locked container is unrelated.
                continue;
            }
            continue;
        };

        let mut relevant_incoming = BTreeSet::new();
        let mut relevant_outgoing = BTreeSet::new();

        let incoming_from_this_container: Vec<_> = current_incoming
            .iter()
            .copied()
            .filter(|record| record.linker_uid == corresponding.uid)
            .collect();
        detect_duplicate_incoming(
            current,
            key,
            &corresponding.uid,
            &corresponding.path,
            &incoming_from_this_container,
            &mut report,
        );

        let corresponding_incoming_candidates: Vec<_> = other
            .incoming
            .iter()
            .filter(|record| {
                record.linker_uid == current.container_uid && record.linker_key == *key
            })
            .collect();
        detect_duplicate_incoming(
            current,
            key,
            &corresponding.uid,
            &corresponding.path,
            &corresponding_incoming_candidates,
            &mut report,
        );

        // Validate every incoming record in the current container which claims
        // to originate from this corresponding container.
        for incoming in current_incoming
            .iter()
            .filter(|record| record.linker_uid == corresponding.uid)
        {
            let candidates: Vec<_> = other
                .outgoing
                .iter()
                .enumerate()
                .filter(|(_, record)| record.linker_key == incoming.linker_key)
                .collect();
            if candidates.is_empty() {
                let mismatched_keys = other
                    .outgoing
                    .iter()
                    .enumerate()
                    .filter(|(_, outgoing)| {
                        outgoing.target_key == *key
                            && (outgoing.target_container_uid == current.container_uid
                                || recorded_path_uid(other, outgoing)
                                    .is_some_and(|uid| uid == current.container_uid))
                    })
                    .collect::<Vec<_>>();
                if mismatched_keys.is_empty() {
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::MissingOutgoingRecord,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&incoming.linker_key),
                        Some(key),
                        Some("matching outgoing record"),
                        None,
                    );
                } else {
                    for (index, outgoing) in mismatched_keys {
                        relevant_outgoing.insert(index);
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::LinkerKeyMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            Some(&incoming.linker_key),
                            Some(&outgoing.linker_key),
                        );
                        validate_recorded_container_path(
                            other,
                            outgoing,
                            &current.container_uid,
                            &current_path,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            &mut report,
                        );
                        validate_materialized_symlink(
                            other,
                            outgoing,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            &mut report,
                        );
                    }
                }
                continue;
            }
            if candidates.len() > 1 {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::DuplicateOutgoingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&incoming.linker_key),
                    Some(key),
                    Some("one outgoing record"),
                    Some(&candidates.len().to_string()),
                );
            }
            for (index, outgoing) in candidates {
                relevant_outgoing.insert(index);
                let mut matches = true;
                if outgoing.target_container_uid != current.container_uid {
                    matches = false;
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::TargetUidMismatch,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&outgoing.linker_key),
                        Some(&outgoing.target_key),
                        Some(&current.container_uid),
                        Some(&outgoing.target_container_uid),
                    );
                }
                if outgoing.target_key != *key {
                    matches = false;
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::TargetKeyMismatch,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&outgoing.linker_key),
                        Some(&outgoing.target_key),
                        Some(key),
                        Some(&outgoing.target_key),
                    );
                }
                if !validate_recorded_container_path(
                    other,
                    outgoing,
                    &current.container_uid,
                    &current_path,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    &mut report,
                ) {
                    matches = false;
                }
                if !validate_materialized_symlink(
                    other,
                    outgoing,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    &mut report,
                ) {
                    matches = false;
                }
                if matches {
                    push_match(
                        &mut report,
                        &mut valid_seen,
                        &corresponding.uid,
                        &outgoing.linker_key,
                        &current.container_uid,
                        key,
                    );
                }
            }
        }

        // Find outgoing records which independently claim this current entry,
        // including records whose stored UID is wrong but whose path resolves
        // to the current container.
        for (index, outgoing) in other.outgoing.iter().enumerate() {
            if relevant_outgoing.contains(&index) {
                continue;
            }
            let same_key_claimed = current_incoming
                .iter()
                .any(|record| record.linker_key == outgoing.linker_key);
            let uid_claim = outgoing.target_container_uid == current.container_uid
                && outgoing.target_key == *key;
            let path_claim = outgoing.target_key == *key
                && recorded_path_uid(other, outgoing)
                    .is_some_and(|uid| uid == current.container_uid);
            if !(same_key_claimed || uid_claim || path_claim) {
                continue;
            }
            relevant_outgoing.insert(index);
            if outgoing.target_container_uid != current.container_uid {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::TargetUidMismatch,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    Some(&current.container_uid),
                    Some(&outgoing.target_container_uid),
                );
            }
            if outgoing.target_key != *key {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::TargetKeyMismatch,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    Some(key),
                    Some(&outgoing.target_key),
                );
            }
            validate_recorded_container_path(
                other,
                outgoing,
                &current.container_uid,
                &current_path,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            validate_materialized_symlink(
                other,
                outgoing,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            let claimed_by: Vec<_> = current_incoming
                .iter()
                .filter(|record| record.linker_key == outgoing.linker_key)
                .collect();
            if !claimed_by.is_empty() {
                for record in claimed_by {
                    if record.linker_uid == corresponding.uid {
                        // An exact-UID record would already have consumed this
                        // outgoing key above. Reaching here means it is a duplicate.
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::DuplicateOutgoingRecord,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            None,
                            None,
                        );
                    } else {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::LinkerUidMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            Some(&record.linker_uid),
                            Some(&corresponding.uid),
                        );
                    }
                }
            } else {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::UnexpectedOutgoingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    None,
                    Some("outgoing record has no matching incoming record"),
                );
            }
        }

        // Validate the current entry's outgoing record against all incoming
        // records in the corresponding container.
        for outgoing in current_outgoing
            .iter()
            .filter(|record| record.target_container_uid == corresponding.uid)
        {
            let mut exact = false;
            for (index, incoming) in other.incoming.iter().enumerate() {
                if incoming.linker_uid == current.container_uid && incoming.linker_key == *key {
                    relevant_incoming.insert(index);
                    if incoming.target_key == outgoing.target_key {
                        exact = true;
                    } else {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::TargetKeyMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(key),
                            Some(&incoming.target_key),
                            Some(&outgoing.target_key),
                            Some(&incoming.target_key),
                        );
                    }
                }
            }
            let path_valid = validate_recorded_container_path(
                current,
                outgoing,
                &corresponding.uid,
                &corresponding.path,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            let symlink_valid = validate_materialized_symlink(
                current,
                outgoing,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            if !exact {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::MissingIncomingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(key),
                    Some(&outgoing.target_key),
                    Some("matching incoming record"),
                    None,
                );
            } else if path_valid && symlink_valid {
                push_match(
                    &mut report,
                    &mut valid_seen,
                    &current.container_uid,
                    key,
                    &corresponding.uid,
                    &outgoing.target_key,
                );
            }
        }

        // Incoming records in the corresponding container which independently
        // claim the current linker entry must also have a matching current
        // outgoing record.
        for (index, incoming) in other.incoming.iter().enumerate() {
            if relevant_incoming.contains(&index)
                || incoming.linker_uid != current.container_uid
                || incoming.linker_key != *key
            {
                continue;
            }
            relevant_incoming.insert(index);
            if !current_outgoing.is_empty() {
                for outgoing in &current_outgoing {
                    let kind = if outgoing.target_container_uid != corresponding.uid {
                        LinkValidationIssueKind::TargetUidMismatch
                    } else {
                        LinkValidationIssueKind::TargetKeyMismatch
                    };
                    push_issue(
                        &mut report,
                        kind,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(key),
                        Some(&incoming.target_key),
                        Some(&format!(
                            "{}:{}",
                            outgoing.target_container_uid, outgoing.target_key
                        )),
                        Some(&format!("{}:{}", corresponding.uid, incoming.target_key)),
                    );
                }
            } else {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::UnexpectedIncomingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(key),
                    Some(&incoming.target_key),
                    None,
                    Some("incoming record has no matching outgoing record"),
                );
            }
        }

        report.ignored_corresponding_records +=
            other.incoming.len().saturating_sub(relevant_incoming.len());
        report.ignored_corresponding_records +=
            other.outgoing.len().saturating_sub(relevant_outgoing.len());
        if relevant_incoming.is_empty() && relevant_outgoing.is_empty() && !current_refs_this_uid {
            report.ignored_containers += 1;
        }
    }

    Ok(report)
}

fn normalized_path(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Ok(path);
    }
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolved_recorded_path(owner: &ContainerLinkSnapshot, outgoing: &OutgoingLinkRecord) -> PathBuf {
    if outgoing.target_container_path.is_absolute() {
        outgoing.target_container_path.clone()
    } else {
        owner.container_path.join(&outgoing.target_container_path)
    }
}

fn recorded_path_uid(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
) -> Option<String> {
    open_container(resolved_recorded_path(owner, outgoing))
        .ok()
        .and_then(|container| container.uid().ok())
}

#[allow(clippy::too_many_arguments)]
fn validate_recorded_container_path(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
    expected_uid: &str,
    expected_path: &Path,
    current: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    report: &mut LinkValidationReport,
) -> bool {
    let resolved = resolved_recorded_path(owner, outgoing);
    let container = match open_container(&resolved) {
        Ok(container) => container,
        Err(error) if is_transient_error(&error) => {
            report.unavailable.push(LinkValidationUnavailable {
                container_uid: outgoing.target_container_uid.clone(),
                container_path: resolved,
                reason: error.to_string(),
            });
            return false;
        }
        Err(error) => {
            push_issue(
                report,
                LinkValidationIssueKind::ContainerPathMissing,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(&expected_path.display().to_string()),
                Some(&format!("{} ({error:#})", resolved.display())),
            );
            return false;
        }
    };
    let actual_uid = match container.uid() {
        Ok(uid) => uid,
        Err(error) if is_transient_error(&error) => {
            report.unavailable.push(LinkValidationUnavailable {
                container_uid: outgoing.target_container_uid.clone(),
                container_path: resolved,
                reason: error.to_string(),
            });
            return false;
        }
        Err(error) => {
            push_issue(
                report,
                LinkValidationIssueKind::ContainerPathMissing,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(expected_uid),
                Some(&format!("failed to read UID: {error:#}")),
            );
            return false;
        }
    };
    if actual_uid != expected_uid || actual_uid != outgoing.target_container_uid {
        push_issue(
            report,
            LinkValidationIssueKind::ContainerPathUidMismatch,
            current,
            current_key,
            corresponding_uid,
            corresponding_path,
            Some(&outgoing.linker_key),
            Some(&outgoing.target_key),
            Some(expected_uid),
            Some(&actual_uid),
        );
        return false;
    }
    let normalized_resolved = normalized_path(&resolved).unwrap_or(resolved);
    let normalized_expected =
        normalized_path(expected_path).unwrap_or_else(|_| expected_path.into());
    if normalized_resolved != normalized_expected {
        push_issue(
            report,
            LinkValidationIssueKind::ContainerPathMismatch,
            current,
            current_key,
            corresponding_uid,
            corresponding_path,
            Some(&outgoing.linker_key),
            Some(&outgoing.target_key),
            Some(&normalized_expected.display().to_string()),
            Some(&normalized_resolved.display().to_string()),
        );
        return false;
    }
    let target_path = match container.filepath(&outgoing.target_key) {
        Ok(path) => path,
        Err(error) if is_transient_error(&error) => {
            report.unavailable.push(LinkValidationUnavailable {
                container_uid: actual_uid,
                container_path: normalized_resolved,
                reason: error.to_string(),
            });
            return false;
        }
        Err(error) => {
            push_issue(
                report,
                LinkValidationIssueKind::TargetEntryMissing,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some("existing ordinary target entry"),
                Some(&error.to_string()),
            );
            return false;
        }
    };
    if !target_path.is_file() {
        push_issue(
            report,
            LinkValidationIssueKind::TargetEntryMissing,
            current,
            current_key,
            corresponding_uid,
            corresponding_path,
            Some(&outgoing.linker_key),
            Some(&outgoing.target_key),
            Some("existing ordinary target entry"),
            Some(&target_path.display().to_string()),
        );
        return false;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn validate_materialized_symlink(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
    current: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    report: &mut LinkValidationReport,
) -> bool {
    let link_path = owner.container_path.join(&outgoing.linker_key);
    let target_path = resolved_recorded_path(owner, outgoing).join(&outgoing.target_key);
    let expected = if outgoing.target_container_path.is_relative() {
        link_path
            .parent()
            .and_then(|parent| pathdiff::diff_paths(&target_path, parent))
            .unwrap_or(target_path)
    } else {
        target_path
    };

    let metadata = match std::fs::symlink_metadata(&link_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            push_issue(
                report,
                LinkValidationIssueKind::MaterializedSymlinkMissing,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(&expected.display().to_string()),
                None,
            );
            return false;
        }
        Err(error) if is_transient_io_kind(error.kind()) => {
            report.unavailable.push(LinkValidationUnavailable {
                container_uid: owner.container_uid.clone(),
                container_path: owner.container_path.clone(),
                reason: format!("failed to inspect {}: {error}", link_path.display()),
            });
            return false;
        }
        Err(error) => {
            push_issue(
                report,
                LinkValidationIssueKind::MaterializedSymlinkMismatch,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(&expected.display().to_string()),
                Some(&error.to_string()),
            );
            return false;
        }
    };
    if !metadata.file_type().is_symlink() {
        push_issue(
            report,
            LinkValidationIssueKind::MaterializedSymlinkMismatch,
            current,
            current_key,
            corresponding_uid,
            corresponding_path,
            Some(&outgoing.linker_key),
            Some(&outgoing.target_key),
            Some(&expected.display().to_string()),
            Some("filesystem entry is not a symlink"),
        );
        return false;
    }
    match std::fs::read_link(&link_path) {
        Ok(actual) if actual == expected => true,
        Ok(actual) => {
            push_issue(
                report,
                LinkValidationIssueKind::MaterializedSymlinkMismatch,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(&expected.display().to_string()),
                Some(&actual.display().to_string()),
            );
            false
        }
        Err(error) if is_transient_io_kind(error.kind()) => {
            report.unavailable.push(LinkValidationUnavailable {
                container_uid: owner.container_uid.clone(),
                container_path: owner.container_path.clone(),
                reason: format!("failed to read symlink {}: {error}", link_path.display()),
            });
            false
        }
        Err(error) => {
            push_issue(
                report,
                LinkValidationIssueKind::MaterializedSymlinkMismatch,
                current,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&outgoing.linker_key),
                Some(&outgoing.target_key),
                Some(&expected.display().to_string()),
                Some(&error.to_string()),
            );
            false
        }
    }
}

fn is_transient_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<WriterError>(),
            Some(WriterError::ContainerLocked)
        ) || cause.downcast_ref::<link::LinkUnavailableError>().is_some()
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| is_transient_io_kind(error.kind()))
    })
}

fn is_unavailable_snapshot_error(error: &anyhow::Error) -> bool {
    is_transient_error(error)
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<LinkMetadataMigrationError>().is_some())
}

fn is_transient_io_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}

#[allow(clippy::too_many_arguments)]
fn push_issue(
    report: &mut LinkValidationReport,
    kind: LinkValidationIssueKind,
    current: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    linker_key: Option<&str>,
    target_key: Option<&str>,
    expected: Option<&str>,
    actual: Option<&str>,
) {
    report.broken.push(LinkValidationIssue {
        kind,
        current_container_uid: current.container_uid.clone(),
        current_key: current_key.clone(),
        corresponding_container_uid: corresponding_uid.to_owned(),
        corresponding_container_path: corresponding_path.to_owned(),
        linker_key: linker_key.map(ToOwned::to_owned),
        target_key: target_key.map(ToOwned::to_owned),
        expected: expected.map(ToOwned::to_owned),
        actual: actual.map(ToOwned::to_owned),
    });
}

fn push_match(
    report: &mut LinkValidationReport,
    seen: &mut BTreeSet<(String, String, String, String)>,
    linker_uid: &str,
    linker_key: &str,
    target_uid: &str,
    target_key: &str,
) {
    let identity = (
        linker_uid.to_owned(),
        linker_key.to_owned(),
        target_uid.to_owned(),
        target_key.to_owned(),
    );
    if seen.insert(identity.clone()) {
        report.valid.push(LinkMatch {
            linker_container_uid: identity.0,
            linker_key: identity.1,
            target_container_uid: identity.2,
            target_key: identity.3,
        });
    }
}

fn detect_duplicate_incoming(
    owner: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    records: &[&IncomingLinkRecord],
    report: &mut LinkValidationReport,
) {
    let mut seen = BTreeSet::new();
    for record in records {
        let identity = (
            record.target_key.clone(),
            record.linker_uid.clone(),
            record.linker_key.clone(),
        );
        if !seen.insert(identity) {
            push_issue(
                report,
                LinkValidationIssueKind::DuplicateIncomingRecord,
                owner,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&record.linker_key),
                Some(&record.target_key),
                None,
                None,
            );
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
