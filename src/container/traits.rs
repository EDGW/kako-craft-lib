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

    /// Atomically replaces one reciprocal incoming linker key in a single
    /// metadata write. This is used by outgoing-link rename transactions.
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
