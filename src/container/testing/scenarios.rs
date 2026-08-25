//! Isolated corruption scenarios used by the broken-container fixture builder.

use std::fs;
use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::BrokenFixtureErrorKind;
use super::broken::{FixtureBuilder, LINKER_KEY, TARGET_KEY, validation};
use super::filesystem::{
    absolute, create_file_symlink, incoming_path, object_mut, outgoing_path, read_json,
    remove_incoming, remove_outgoing, remove_symlink, update_incoming_record,
    update_outgoing_record, write_json,
};
use crate::container::{
    CONTAINER_METADATA_FILE, CONTROL_DIR, Container, LinkValidationIssueKind, LocalContainer,
};

impl FixtureBuilder {
    /// Generates missing-outgoing and unexpected-incoming views of one broken relationship.
    ///
    /// # Returns
    ///
    /// `Ok(())` after removing the outgoing record and symlink and registering both deferred views.
    ///
    /// # Errors
    ///
    /// Returns an error if the valid pair cannot be created or its outgoing metadata or symlink
    /// cannot be read, changed, or persisted.
    pub(super) fn missing_outgoing_and_unexpected_incoming(&mut self) -> Result<()> {
        let pair = self.pair("missing-outgoing")?;
        remove_outgoing(&pair.linker)?;
        remove_symlink(&pair.linker.join(LINKER_KEY))?;
        self.case(
            "missing-outgoing-record",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::MissingOutgoingRecord)],
        );
        self.case(
            "unexpected-incoming-record",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![validation(
                LinkValidationIssueKind::UnexpectedIncomingRecord,
            )],
        );
        Ok(())
    }

    /// Generates missing-incoming and unexpected-outgoing views of one broken relationship.
    ///
    /// # Returns
    ///
    /// `Ok(())` after removing reciprocal incoming metadata and registering both deferred views.
    ///
    /// # Errors
    ///
    /// Returns an error if the valid pair cannot be created or incoming metadata cannot be read,
    /// changed, or persisted.
    pub(super) fn missing_incoming_and_unexpected_outgoing(&mut self) -> Result<()> {
        let pair = self.pair("missing-incoming")?;
        remove_incoming(&pair.target)?;
        self.case(
            "missing-incoming-record",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![validation(LinkValidationIssueKind::MissingIncomingRecord)],
        );
        self.case(
            "unexpected-outgoing-record",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(
                LinkValidationIssueKind::UnexpectedOutgoingRecord,
            )],
        );
        Ok(())
    }

    /// Replaces an incoming source UID while retaining its linker key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after creating an imposter container, persisting its UID in the reciprocal record,
    /// and registering the deferred mismatch case.
    ///
    /// # Errors
    ///
    /// Returns an error if pair or imposter creation, UID access, or incoming metadata mutation
    /// fails.
    pub(super) fn linker_uid_mismatch(&mut self) -> Result<()> {
        let pair = self.pair("linker-uid-mismatch")?;
        let imposter_path = self.root.join("linker-uid-mismatch/imposter");
        let imposter = LocalContainer::with_logical_name(&imposter_path, "uid-imposter")?;
        let imposter_uid = imposter.uid()?;
        self.containers.push(imposter_path);
        update_incoming_record(&pair.target, |record| {
            record["linker_uid"] = Value::String(imposter_uid);
            Ok(())
        })?;
        self.case(
            "linker-uid-mismatch",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::LinkerUidMismatch)],
        );
        Ok(())
    }

    /// Replaces the recorded target UID while retaining a path to the original target.
    ///
    /// # Returns
    ///
    /// `Ok(())` after an imposter UID is stored and the target-UID and path-UID case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair or imposter creation, UID access, or outgoing metadata mutation
    /// fails.
    pub(super) fn target_uid_and_path_uid_mismatch(&mut self) -> Result<()> {
        let pair = self.pair("target-uid-mismatch")?;
        let imposter_path = self.root.join("target-uid-mismatch/imposter");
        let imposter = LocalContainer::with_logical_name(&imposter_path, "target-uid-imposter")?;
        let imposter_uid = imposter.uid()?;
        self.containers.push(imposter_path);
        update_outgoing_record(&pair.linker, |record| {
            record["container_uid"] = Value::String(imposter_uid);
            Ok(())
        })?;
        self.case(
            "target-and-container-path-uid-mismatch",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![
                validation(LinkValidationIssueKind::TargetUidMismatch),
                validation(LinkValidationIssueKind::ContainerPathUidMismatch),
            ],
        );
        Ok(())
    }

    /// Renames only the outgoing map key and materialized symlink.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the outgoing record and symlink use a key different from the retained incoming
    /// record and the deferred case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation, JSON access or persistence, or symlink rename fails.
    pub(super) fn linker_key_mismatch(&mut self) -> Result<()> {
        let pair = self.pair("linker-key-mismatch")?;
        let replacement = "actual-link.txt";
        let mut metadata = read_json(&outgoing_path(&pair.linker))?;
        let links = object_mut(&mut metadata, "links")?;
        let record = links
            .remove(LINKER_KEY)
            .ok_or_else(|| anyhow!("generated outgoing record is missing"))?;
        links.insert(replacement.to_owned(), record);
        write_json(&outgoing_path(&pair.linker), &metadata)?;
        fs::rename(pair.linker.join(LINKER_KEY), pair.linker.join(replacement))?;
        self.case(
            "linker-key-mismatch",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::LinkerKeyMismatch)],
        );
        Ok(())
    }

    /// Replaces the recorded target key with an absent key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the wrong key is persisted and the key-mismatch case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation or outgoing metadata mutation fails.
    pub(super) fn target_key_and_missing_entry(&mut self) -> Result<()> {
        let pair = self.pair("target-key-mismatch")?;
        update_outgoing_record(&pair.linker, |record| {
            record["target_key"] = Value::String("wrong-target.txt".to_owned());
            Ok(())
        })?;
        self.case(
            "target-key-mismatch",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![
                validation(LinkValidationIssueKind::TargetKeyMismatch),
                validation(LinkValidationIssueKind::TargetEntryMissing),
            ],
        );
        Ok(())
    }

    /// Deletes an otherwise correctly recorded ordinary target file.
    ///
    /// # Returns
    ///
    /// `Ok(())` after deleting the target file directly and registering the deferred case.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation or direct target-file deletion fails.
    pub(super) fn target_entry_missing(&mut self) -> Result<()> {
        let pair = self.pair("target-entry-missing")?;
        fs::remove_file(pair.target.join(TARGET_KEY))?;
        self.case(
            "target-entry-missing",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![validation(LinkValidationIssueKind::TargetEntryMissing)],
        );
        Ok(())
    }

    /// Replaces the recorded target path with a nonexistent absolute path.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the missing path is persisted and the deferred case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation, absolute-path resolution, serialization, or outgoing
    /// metadata persistence fails.
    pub(super) fn container_path_missing(&mut self) -> Result<()> {
        let pair = self.pair("container-path-missing")?;
        let missing = absolute(&self.root.join("container-path-missing/does-not-exist"))?;
        update_outgoing_record(&pair.linker, |record| {
            record["container_path"] = serde_json::to_value(missing)?;
            Ok(())
        })?;
        self.case(
            "container-path-missing",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::ContainerPathMissing)],
        );
        Ok(())
    }

    /// Points an outgoing record at a different root carrying the target's copied UID.
    ///
    /// # Returns
    ///
    /// `Ok(())` after creating the duplicate-UID target root, redirecting outgoing metadata, and
    /// registering the path-mismatch case.
    ///
    /// # Errors
    ///
    /// Returns an error if pair or clone creation, metadata or entry copying, path resolution,
    /// serialization, or outgoing metadata persistence fails.
    pub(super) fn container_path_mismatch(&mut self) -> Result<()> {
        let pair = self.pair("container-path-mismatch")?;
        let clone_path = self.root.join("container-path-mismatch/target-clone");
        LocalContainer::with_logical_name(&clone_path, "target-clone")?;
        fs::copy(
            pair.target.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE),
            clone_path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE),
        )?;
        fs::write(clone_path.join(TARGET_KEY), b"cloned fixture target\n")?;
        self.containers.push(clone_path.clone());
        let clone_absolute = absolute(&clone_path)?;
        update_outgoing_record(&pair.linker, |record| {
            record["container_path"] = serde_json::to_value(clone_absolute)?;
            Ok(())
        })?;
        self.case(
            "container-path-mismatch",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![validation(LinkValidationIssueKind::ContainerPathMismatch)],
        );
        Ok(())
    }

    /// Duplicates the exact reciprocal incoming record and updates its cached count.
    ///
    /// # Returns
    ///
    /// `Ok(())` after two identical records and the matching count are persisted and the deferred
    /// case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation, incoming JSON access, or persistence fails.
    pub(super) fn duplicate_incoming(&mut self) -> Result<()> {
        let pair = self.pair("duplicate-incoming")?;
        let path = incoming_path(&pair.target);
        let mut metadata = read_json(&path)?;
        let records = metadata["links"][TARGET_KEY]
            .as_array_mut()
            .ok_or_else(|| anyhow!("generated incoming record is not an array"))?;
        let record = records
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("generated incoming record is missing"))?;
        records.push(record);
        metadata["count"] = Value::from(2_u64);
        write_json(&path, &metadata)?;
        self.case(
            "duplicate-incoming-record",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::DuplicateIncomingRecord)],
        );
        Ok(())
    }

    /// Replaces a referenced linker's outgoing metadata with invalid JSON.
    ///
    /// # Returns
    ///
    /// `Ok(())` after malformed bytes replace the document and the deferred case is registered.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation or writing the malformed metadata fails.
    pub(super) fn metadata_invalid(&mut self) -> Result<()> {
        let pair = self.pair("metadata-invalid")?;
        fs::write(
            outgoing_path(&pair.linker),
            b"{ deliberately invalid json\n",
        )?;
        self.case(
            "metadata-invalid",
            &pair.target,
            TARGET_KEY,
            &[&pair.linker],
            vec![validation(LinkValidationIssueKind::MetadataInvalid)],
        );
        Ok(())
    }

    /// Removes an otherwise valid outgoing symlink.
    ///
    /// # Returns
    ///
    /// `Ok(())` after removing the symlink and registering both validation and check expectations.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation, symlink inspection, or symlink removal fails.
    pub(super) fn missing_symlink(&mut self) -> Result<()> {
        let pair = self.pair("missing-symlink")?;
        remove_symlink(&pair.linker.join(LINKER_KEY))?;
        self.case(
            "missing-symlink",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![
                validation(LinkValidationIssueKind::MaterializedSymlinkMissing),
                BrokenFixtureErrorKind::MissingSymlink,
            ],
        );
        Ok(())
    }

    /// Replaces an otherwise valid outgoing symlink with the wrong target text.
    ///
    /// # Returns
    ///
    /// `Ok(())` after installing the incorrect symlink and registering validation and check
    /// expectations.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation or symbolic-link removal or creation fails.
    pub(super) fn incorrect_symlink(&mut self) -> Result<()> {
        let pair = self.pair("incorrect-symlink")?;
        let link = pair.linker.join(LINKER_KEY);
        remove_symlink(&link)?;
        create_file_symlink(Path::new("deliberately-wrong-target"), &link)?;
        self.case(
            "incorrect-symlink",
            &pair.linker,
            LINKER_KEY,
            &[&pair.target],
            vec![
                validation(LinkValidationIssueKind::MaterializedSymlinkMismatch),
                BrokenFixtureErrorKind::IncorrectSymlink,
            ],
        );
        Ok(())
    }

    /// Adds a symlink for which no outgoing record exists.
    ///
    /// # Returns
    ///
    /// `Ok(())` after creating the orphan symlink and registering its deferred check case.
    ///
    /// # Errors
    ///
    /// Returns an error if pair creation or orphan symlink creation fails.
    pub(super) fn unrecorded_symlink(&mut self) -> Result<()> {
        let pair = self.pair("unrecorded-symlink")?;
        let orphan = "orphan-link.txt";
        create_file_symlink(Path::new("orphan-target.txt"), &pair.linker.join(orphan))?;
        self.case(
            "unrecorded-symlink",
            &pair.linker,
            orphan,
            &[&pair.target],
            vec![BrokenFixtureErrorKind::UnrecordedSymlink],
        );
        Ok(())
    }
}
