//! Locked outgoing-link access validation.

use std::path::{Path, PathBuf};

use super::super::{
    BrokenLinkError, ContainerWriteGuard, EntryKey, LinkAccessError, LinkUnavailableError,
    open_container,
};
use super::filesystem::{resolved_container_path, verify_materialized_symlink};
use super::metadata::OutgoingLink;

/// Validates an outgoing record and acquires its target container writer.
///
/// # Arguments
///
/// * `root` - Root of the link container owning the outgoing record.
/// * `key` - Linker entry key used in diagnostics and reciprocal-record lookup.
/// * `linker_uid` - Persistent UID of the owning link container.
/// * `link_path` - Materialized symlink path for `key`.
/// * `link` - Outgoing metadata record identifying the target container and entry.
///
/// # Returns
///
/// A write guard for the verified target container together with its resolved target-entry path.
/// The target guard remains locked for the caller's subsequent read or write.
///
/// # Errors
///
/// Returns [`LinkAccessError::Broken`] when the target path, UID, key, reciprocal record, or symlink
/// is inconsistent, and [`LinkAccessError::Unavailable`] when the verified target writer cannot be
/// acquired.
pub(super) fn validate_outgoing_link_locked(
    root: &Path,
    key: &EntryKey,
    linker_uid: &str,
    link_path: &Path,
    link: &OutgoingLink,
) -> std::result::Result<(Box<dyn ContainerWriteGuard>, PathBuf), LinkAccessError> {
    let container_path = resolved_container_path(root, link);

    // TODO(container-pool): resolve the target by container UID first, then
    // verify that the recorded container path still identifies that container.
    let container = open_container(&container_path).map_err(|error| {
        BrokenLinkError::new(
            key,
            format!(
                "failed to open target container {}: {error:#}",
                container_path.display()
            ),
        )
    })?;
    let actual_uid = container.uid().map_err(|error| {
        BrokenLinkError::new(
            key,
            format!("failed to read target container UID: {error:#}"),
        )
    })?;
    if actual_uid != link.container_uid {
        return Err(BrokenLinkError::new(
            key,
            format!(
                "target container UID mismatch: recorded {}, found {} at {}",
                link.container_uid,
                actual_uid,
                container_path.display()
            ),
        )
        .into());
    }
    let target_guard = container.writer().map_err(|error| LinkUnavailableError {
        key: key.clone(),
        container_uid: link.container_uid.clone(),
        container_path: container_path.clone(),
        reason: error.to_string(),
    })?;
    let target_path = validate_outgoing_link_with_guard(
        root,
        key,
        linker_uid,
        link_path,
        link,
        target_guard.as_ref(),
    )?;
    Ok((target_guard, target_path))
}

/// Validates one outgoing record against an already locked target Container.
///
/// # Arguments
///
/// * `root` - Root of the link owner.
/// * `key` - Outgoing linker key.
/// * `linker_uid` - Stable UID of the link owner.
/// * `link_path` - Materialized symlink path for `key`.
/// * `link` - Authoritative outgoing metadata record.
/// * `target_guard` - Already acquired target writer selected by a multi-lock transaction.
///
/// # Returns
///
/// The validated ordinary target-entry filesystem path.
///
/// # Errors
///
/// Returns [`LinkAccessError::Broken`] when target UID/key, reciprocal metadata,
/// or the materialized symlink contradicts the outgoing record.
pub(super) fn validate_outgoing_link_with_guard(
    root: &Path,
    key: &EntryKey,
    linker_uid: &str,
    link_path: &Path,
    link: &OutgoingLink,
    target_guard: &dyn ContainerWriteGuard,
) -> std::result::Result<PathBuf, LinkAccessError> {
    if target_guard.container_uid() != link.container_uid {
        return Err(BrokenLinkError::new(
            key,
            format!(
                "target container UID mismatch: recorded {}, found {}",
                link.container_uid,
                target_guard.container_uid()
            ),
        )
        .into());
    }
    let target_path = target_guard.filepath(&link.target_key).map_err(|error| {
        BrokenLinkError::new(
            key,
            format!(
                "failed to resolve target key '{}': {error:#}",
                link.target_key
            ),
        )
    })?;
    if !target_path.is_file() {
        return Err(BrokenLinkError::new(
            key,
            format!("target key does not exist: {}", link.target_key),
        )
        .into());
    }
    if !target_guard
        .has_link_from(&link.target_key, linker_uid, key)
        .map_err(|error| {
            BrokenLinkError::new(
                key,
                format!("failed to validate reciprocal link: {error:#}"),
            )
        })?
    {
        return Err(
            BrokenLinkError::new(key, "target container has no matching reciprocal link").into(),
        );
    }
    verify_materialized_symlink(root, link_path, link, link.container_path.is_relative())
        .map_err(|error| BrokenLinkError::new(key, error.to_string()))?;
    Ok(target_path)
}
