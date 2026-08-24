use std::path::{Path, PathBuf};

use anyhow::Result;

use super::super::LinkUnavailableError;
use super::super::{
    ContainerLinkSnapshot, EntryKey, LinkMetadataMigrationError, LinkValidationIssueKind,
    LinkValidationReport, LinkValidationUnavailable, OutgoingLinkRecord, WriterError,
    open_container,
};
use super::report::push_issue;

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

pub(crate) fn recorded_path_uid(
    owner: &ContainerLinkSnapshot,
    outgoing: &OutgoingLinkRecord,
) -> Option<String> {
    open_container(resolved_recorded_path(owner, outgoing))
        .ok()
        .and_then(|container| container.uid().ok())
}

#[allow(clippy::too_many_arguments)]
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

pub(crate) fn is_unavailable_snapshot_error(error: &anyhow::Error) -> bool {
    is_transient_error(error)
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<LinkMetadataMigrationError>().is_some())
}

pub(crate) fn is_transient_io_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}
