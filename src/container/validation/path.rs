//! Recorded container-path, target-entry, and materialized-symlink validation.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::super::LinkUnavailableError;
use super::super::{
    ContainerLinkSnapshot, EntryKey, LinkMetadataMigrationError, LinkValidationIssueKind,
    LinkValidationReport, LinkValidationUnavailable, OutgoingLinkRecord, WriterError,
    open_container,
};
use super::report::push_issue;

/// Produces a comparable absolute path, canonicalizing it when possible.
///
/// # Arguments
///
/// * `path` - Container or entry path to normalize.
///
/// # Returns
///
/// The canonical path when it exists and is accessible, otherwise the absolute input unchanged or
/// a relative input joined to the process working directory.
///
/// # Errors
///
/// Returns an error only when canonicalization failed, `path` is relative, and the current working
/// directory cannot be read.
pub(crate) fn normalized_path(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Ok(path);
    }
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Resolves an outgoing record's target-container path from its owner snapshot.
///
/// # Arguments
///
/// * `owner` - Snapshot of the link container that owns `outgoing`.
/// * `outgoing` - Record containing an absolute or owner-relative target-container path.
///
/// # Returns
///
/// An absolute record unchanged, or the owner root joined with a relative record.
pub(crate) fn resolved_recorded_path(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
) -> PathBuf {
    if outgoing.target_container_path.is_absolute() {
        outgoing.target_container_path.clone()
    } else {
        owner.container_path.join(&outgoing.target_container_path)
    }
}

/// Best-effort reads the UID of the container identified by an outgoing record's path.
///
/// # Arguments
///
/// * `owner` - Snapshot providing the base for a relative recorded path.
/// * `outgoing` - Outgoing record whose target path should be inspected.
///
/// # Returns
///
/// `Some(uid)` when the resolved path opens and its UID is readable; otherwise `None`. Failures are
/// intentionally suppressed because this helper only broadens mismatch detection.
pub(crate) fn recorded_path_uid(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
) -> Option<String> {
    open_container(resolved_recorded_path(owner, outgoing))
        .ok()
        .and_then(|container| container.uid().ok())
}

#[allow(clippy::too_many_arguments)]
/// Validates an outgoing record's target path, UID, and ordinary target entry.
///
/// # Arguments
///
/// * `owner` - Snapshot of the container owning `outgoing`, used to resolve relative paths.
/// * `outgoing` - Outgoing record whose recorded path and target entry are checked.
/// * `expected_uid` - UID required for the corresponding target container.
/// * `expected_path` - Expected filesystem root of that target container.
/// * `current` - Snapshot for the validation subject recorded in generated issues.
/// * `current_key` - Subject entry key recorded in generated issues.
/// * `corresponding_uid` - UID attributed to the peer in generated issues.
/// * `corresponding_path` - Peer root attributed to generated issues.
/// * `report` - Report receiving broken or temporarily unavailable results.
///
/// # Returns
///
/// `true` only when the recorded path opens the expected UID at `expected_path` and its target key
/// resolves to an existing regular file; otherwise appends a report item and returns `false`.
pub(crate) fn validate_recorded_container_path(
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
/// Validates the filesystem symlink materialized for an outgoing record.
///
/// # Arguments
///
/// * `owner` - Snapshot whose root contains the materialized symlink and resolves target paths.
/// * `outgoing` - Outgoing record used to derive the linker path and expected target text.
/// * `current` - Snapshot for the validation subject recorded in generated issues.
/// * `current_key` - Subject entry key recorded in generated issues.
/// * `corresponding_uid` - UID attributed to the peer in generated issues.
/// * `corresponding_path` - Peer root attributed to generated issues.
/// * `report` - Report receiving mismatch, missing, or availability results.
///
/// # Returns
///
/// `true` only when the linker path is a symlink whose stored target exactly matches the record;
/// otherwise appends a report item and returns `false`.
pub(crate) fn validate_materialized_symlink(
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

/// Classifies a chained error as temporary for validation reporting.
///
/// # Arguments
///
/// * `error` - Arbitrary error whose complete source chain is inspected.
///
/// # Returns
///
/// `true` for container lock contention, explicit link unavailability, or transient I/O kinds;
/// otherwise `false`.
pub(crate) fn is_transient_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<WriterError>(),
            Some(WriterError::ContainerLocked)
        ) || cause.downcast_ref::<LinkUnavailableError>().is_some()
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| is_transient_io_kind(error.kind()))
    })
}

/// Classifies an error that prevents obtaining a usable peer link snapshot.
///
/// # Arguments
///
/// * `error` - Snapshot error whose complete source chain is inspected.
///
/// # Returns
///
/// `true` for transient errors or link-metadata migration failures; otherwise `false`.
pub(crate) fn is_unavailable_snapshot_error(error: &anyhow::Error) -> bool {
    is_transient_error(error)
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<LinkMetadataMigrationError>().is_some())
}

/// Classifies operating-system I/O kinds that may succeed on a later validation attempt.
///
/// # Arguments
///
/// * `kind` - Standard I/O error kind to classify.
///
/// # Returns
///
/// `true` for permission denial, would-block, interruption, or timeout; otherwise `false`.
pub(crate) fn is_transient_io_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}
