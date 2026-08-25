//! Read-only container handles and lock-owning mutation guard traits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use super::check::{apply_validation_check_action, validation_check_issues};
use super::validation::report::{
    log_validation_report, merge_validation_report, sort_validation_report,
};
use super::validation::{validate_link_snapshots, validate_recorded_link_snapshots};
use super::{
    AddError, Buffer, CheckActionResult, CheckRepairAction, ContainerEntryInfo,
    ContainerLinkSnapshot, ContainerMetadata, CopyError, EntryKey, LinkCheckIssue, LinkFromError,
    LinkInfo, LinkValidationReport, LinkValidationRunError, RemoveError, RenameError, UnlinkError,
    WriteError, WriterError,
};

/// Read-only interface shared by local and link containers.
///
/// Mutations are intentionally absent; callers must acquire [`Self::writer`]
/// and perform them through [`ContainerWriteGuard`].
pub trait Container {
    /// Returns a copy of the container's persisted common metadata.
    ///
    /// # Returns
    ///
    /// The format version, stable UID, logical name, and concrete kind.
    fn metadata(&self) -> ContainerMetadata;

    /// Returns the container's stable identity.
    ///
    /// # Returns
    ///
    /// A newly owned UID string read from common metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the implementation cannot obtain valid metadata.
    fn uid(&self) -> Result<String> {
        Ok(self.metadata().uid)
    }

    /// Returns the container's user-facing logical name.
    ///
    /// # Returns
    ///
    /// A newly owned logical-name string from common metadata.
    fn logical_name(&self) -> String {
        self.metadata().logical_name
    }

    /// Returns the concrete container kind.
    ///
    /// # Returns
    ///
    /// A newly owned kind string, currently `local` or `link`.
    fn kind(&self) -> String {
        self.metadata().kind
    }

    /// Returns the filesystem root used to reopen and validate this container.
    ///
    /// # Returns
    ///
    /// The container root as an owned path. The path is not necessarily
    /// canonicalized.
    fn root_path(&self) -> PathBuf;

    /// Lists all keys visible through this container interface.
    ///
    /// For a local container this is every ordinary entry; for a link
    /// container it includes both ordinary entries and outgoing-link keys.
    ///
    /// # Returns
    ///
    /// Entry keys in deterministic lexical order.
    ///
    /// # Errors
    ///
    /// Returns an error if the container directory or link metadata cannot be
    /// read.
    fn list(&self) -> Result<Vec<EntryKey>>;

    /// Lists visible entries together with filesystem and raw link metadata.
    ///
    /// # Returns
    ///
    /// One [`ContainerEntryInfo`] per key returned by [`Self::list`], in the
    /// same deterministic order.
    ///
    /// # Errors
    ///
    /// Returns an error when listing fails, the write lock cannot be acquired,
    /// or an entry's raw path or link metadata cannot be read.
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
    /// Resolves the filesystem path occupied by an entry.
    ///
    /// Link-container implementations validate outgoing metadata, target
    /// identity, reciprocal records, and the symbolic link before returning.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key relative to this container root.
    ///
    /// # Returns
    ///
    /// The ordinary file path or validated outgoing symbolic-link path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid keys, missing entries, broken links,
    /// unavailable target locks, or unreadable metadata.
    fn filepath(&self, key: &EntryKey) -> Result<PathBuf>;

    /// Reads the bytes stored at an ordinary entry or validated outgoing link.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key relative to this container root.
    ///
    /// # Returns
    ///
    /// A newly allocated byte buffer containing the entry data.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or missing entries, failed reads, broken
    /// links, unavailable targets, or unreadable metadata.
    fn read(&self, key: &EntryKey) -> Result<Buffer>;
    /// Acquires the non-blocking write lock for this container.
    ///
    /// # Returns
    ///
    /// A boxed guard which owns the lock until dropped.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::ContainerLocked`] if another writer owns the
    /// lock, or [`WriterError::Other`] if lock setup fails.
    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError>;
}

/// Lock-owning interface for container mutation, validation, and checked repair.
///
/// Every method executes while the current container's write lock remains
/// owned by the guard.
pub trait ContainerWriteGuard {
    /// Returns the stable UID of the locked container.
    ///
    /// # Returns
    ///
    /// A borrowed UID valid for the guard's lifetime.
    fn container_uid(&self) -> &str;

    /// Captures all raw incoming and outgoing metadata under the current lock.
    ///
    /// # Returns
    ///
    /// An unvalidated [`ContainerLinkSnapshot`] for set-based validation.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read, parsed, validated, or
    /// migrated safely.
    fn link_snapshot(&self) -> Result<ContainerLinkSnapshot>;

    /// Resolves an entry path while applying the implementation's validation rules.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key relative to the locked container root.
    ///
    /// # Returns
    ///
    /// The ordinary file path or validated outgoing symbolic-link path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or missing entries, broken links,
    /// unavailable targets, or unreadable metadata.
    fn filepath(&self, key: &EntryKey) -> Result<PathBuf>;

    /// Resolves the entry's path without following or validating an outgoing
    /// link. This is used for complete list reports under an existing writer.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key relative to the locked container root.
    ///
    /// # Returns
    ///
    /// The raw filesystem path occupied by the key.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is invalid or its raw path cannot be
    /// resolved.
    fn entry_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.filepath(key)
    }

    /// Reads entry bytes under the current lock.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key relative to the locked container root.
    ///
    /// # Returns
    ///
    /// A newly allocated buffer containing ordinary or validated linked data.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or missing entries, failed reads, broken
    /// links, unavailable targets, or unreadable metadata.
    fn read(&self, key: &EntryKey) -> Result<Buffer>;

    /// Checks link metadata and filesystem links while this write lock is held.
    /// Local containers have no outgoing-link checks and return an empty list.
    ///
    /// # Arguments
    ///
    /// * `corresponding` - Other containers whose raw records should be
    ///   compared bidirectionally with the locked container.
    ///
    /// # Returns
    ///
    /// Deterministically ordered issues, each carrying its permitted actions.
    ///
    /// # Errors
    ///
    /// Returns an error if current metadata cannot be read or validation setup
    /// is invalid. Temporarily unavailable corresponding containers are
    /// normally represented as issues instead of returned errors.
    fn check(&mut self, corresponding: &[&dyn Container]) -> Result<Vec<LinkCheckIssue>> {
        validation_check_issues(self, corresponding)
    }

    /// Applies one explicitly selected action while this writer remains held.
    ///
    /// # Arguments
    ///
    /// * `issue` - Issue returned by a recent [`Self::check`] call.
    /// * `action` - One action listed in `issue.actions`.
    /// * `corresponding` - Containers available for reciprocal mutations;
    ///   paths recorded in the issue are used when a matching UID is absent.
    ///
    /// # Returns
    ///
    /// A description of the mutation, retry request, or skip that completed.
    ///
    /// # Errors
    ///
    /// Returns [`crate::container::CheckActionError`] for invalid action/input
    /// combinations, or an underlying lock, validation, or persistence error.
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
    ///
    /// # Arguments
    ///
    /// * `key` - New ordinary entry key.
    /// * `data` - Complete bytes to persist for the entry.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the complete entry has been installed.
    ///
    /// # Errors
    ///
    /// Returns [`AddError`] when the key is invalid, already occupied, belongs
    /// to a link, or cannot be persisted.
    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError>;

    /// Creates or replaces an ordinary entry with complete new contents.
    ///
    /// # Arguments
    ///
    /// * `key` - Ordinary entry key to create or replace.
    /// * `data` - Complete replacement bytes.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the complete contents have been installed.
    ///
    /// # Errors
    ///
    /// Returns [`WriteError`] when the key is invalid, belongs to an outgoing
    /// link, or cannot be persisted.
    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError>;

    /// Removes an ordinary entry. Entries participating in either side of a
    /// link must be unlinked before they can be removed.
    ///
    /// # Arguments
    ///
    /// * `key` - Ordinary entry key to remove.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the file is removed.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveError`] when the key is invalid or absent, any link
    /// record protects it, or removal fails.
    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError>;

    /// Preflights a batch before removing any entry. Duplicate keys are
    /// removed once; a validation error leaves the whole batch untouched.
    ///
    /// # Arguments
    ///
    /// * `keys` - Ordinary entry keys to preflight and remove as one guarded
    ///   batch.
    ///
    /// # Returns
    ///
    /// `Ok(())` after every unique requested entry is removed.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveError`] before mutation if any key is invalid, absent,
    /// or linked. A filesystem failure during the removal phase is also
    /// returned, though already completed removals cannot be restored.
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
    ///
    /// # Arguments
    ///
    /// * `from` - Existing ordinary source key.
    /// * `to` - Unoccupied destination key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the filesystem entry has the destination key.
    ///
    /// # Errors
    ///
    /// Returns [`RenameError`] for invalid keys, missing source, occupied
    /// destination, link protection, or filesystem failure.
    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError>;

    /// Copies an ordinary entry without replacing `to`.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing ordinary source key.
    /// * `to` - Unoccupied destination key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after a content copy exists at the destination key.
    ///
    /// # Errors
    ///
    /// Returns [`CopyError`] for invalid keys, missing or linked source,
    /// occupied destination, or filesystem failure.
    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError>;

    /// Registers an exact reciprocal incoming-link source for an ordinary target.
    ///
    /// # Arguments
    ///
    /// * `linker_uid` - Stable UID of the outgoing-link owner.
    /// * `target_key` - Existing ordinary key in the locked target container.
    /// * `linker_key` - Outgoing-link key in the linker container.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the incoming metadata record is persisted.
    ///
    /// # Errors
    ///
    /// Returns [`LinkFromError`] for invalid keys, missing target, duplicate or
    /// conflicting records, or metadata persistence failure.
    fn link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError>;

    /// Removes one exact reciprocal incoming-link source.
    ///
    /// # Arguments
    ///
    /// * `linker_uid` - Stable UID of the outgoing-link owner.
    /// * `target_key` - Ordinary target key owning incoming records.
    /// * `linker_key` - Outgoing-link key to remove from that source.
    ///
    /// # Returns
    ///
    /// `Ok(())` after only the matching source record is removed.
    ///
    /// # Errors
    ///
    /// Returns [`UnlinkError`] for invalid keys, absent or mismatched records,
    /// or metadata persistence failure.
    fn unlink(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError>;

    /// Atomically replaces one reciprocal incoming linker key in a single
    /// metadata write. This is used by outgoing-link rename transactions.
    ///
    /// # Arguments
    ///
    /// * `linker_uid` - Stable UID of the outgoing-link owner.
    /// * `target_key` - Ordinary target key owning the reciprocal record.
    /// * `from` - Existing outgoing-link key in that record.
    /// * `to` - Replacement outgoing-link key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after one metadata write replaces the exact source key.
    ///
    /// # Errors
    ///
    /// Returns [`LinkFromError`] when keys are invalid, the exact source is
    /// absent or ambiguous, the destination is already registered, or
    /// persistence fails.
    fn rename_link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), LinkFromError>;

    /// Recreates only a missing outgoing side during a checked repair. The
    /// reciprocal incoming record must already exist and be protected by the
    /// caller's current writer lock.
    ///
    /// # Arguments
    ///
    /// * `_linker_key` - Outgoing key to recreate in this locked container.
    /// * `_target_container_uid` - UID the target path must identify.
    /// * `_target_container_path` - Target container root used to derive the
    ///   recorded relative or absolute address.
    /// * `_target_key` - Existing ordinary key in the target container.
    ///
    /// # Returns
    ///
    /// `Ok(())` after outgoing metadata and its symbolic link are recreated.
    ///
    /// # Errors
    ///
    /// The default implementation always errors because not every container
    /// kind can create outgoing links. Link containers also return validation
    /// or persistence failures.
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
    ///
    /// # Arguments
    ///
    /// * `_linker_key` - Outgoing key whose local metadata and symbolic link
    ///   should be removed.
    ///
    /// # Returns
    ///
    /// `Ok(())` after local outgoing state is removed.
    ///
    /// # Errors
    ///
    /// The default implementation always errors because not every container
    /// kind owns outgoing state. Link containers return validation or
    /// persistence failures.
    fn repair_remove_outgoing(&mut self, _linker_key: &EntryKey) -> Result<()> {
        Err(anyhow!("this container cannot remove outgoing links"))
    }

    /// Returns raw link metadata associated with one entry key.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key whose incoming or outgoing metadata is requested.
    ///
    /// # Returns
    ///
    /// [`LinkInfo::LinkTo`], [`LinkInfo::LinkFrom`], or [`LinkInfo::None`].
    /// Incoming sources are returned in stable UID/key order.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid keys or unreadable, invalid, or
    /// unmigratable metadata.
    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo>;

    /// Tests whether an exact reciprocal incoming source is registered.
    ///
    /// # Arguments
    ///
    /// * `target_key` - Ordinary target key to inspect.
    /// * `linker_uid` - Stable UID expected for the source container.
    /// * `linker_key` - Outgoing-link key expected in that source container.
    ///
    /// # Returns
    ///
    /// `true` only when the exact UID/key pair is present; otherwise `false`.
    ///
    /// # Errors
    ///
    /// Returns an error when the key or incoming metadata cannot be read.
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

    /// Validates all relevant current and corresponding records for one key.
    ///
    /// The comparison is bidirectional and set-based. Outgoing records whose
    /// target UID is not explicitly supplied are additionally validated by
    /// reopening their recorded path.
    ///
    /// # Arguments
    ///
    /// * `key` - Current-container entry key to validate.
    /// * `corresponding` - Containers whose incoming and outgoing raw records
    ///   should be compared with the current snapshot.
    ///
    /// # Returns
    ///
    /// A complete report containing valid pairs, broken issues, unavailable
    /// containers, and ignored counts. Broken relationships are report data,
    /// not returned errors.
    ///
    /// # Errors
    ///
    /// Returns [`LinkValidationRunError`] for self-validation, ambiguous
    /// duplicate UIDs, or inability to obtain the current snapshot.
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
    ///
    /// # Arguments
    ///
    /// * `key` - Current-container key whose recorded outgoing target should
    ///   be reopened and validated.
    ///
    /// # Returns
    ///
    /// A complete validation report. Keys without outgoing records produce an
    /// empty valid report.
    ///
    /// # Errors
    ///
    /// Returns [`LinkValidationRunError`] when current metadata cannot be read
    /// or validation setup fails.
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
