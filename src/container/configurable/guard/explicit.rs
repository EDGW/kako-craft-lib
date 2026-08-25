//! Explicit rule management and forced local/link operations.

use anyhow::{Result, bail};

use crate::container::{
    AddError, Buffer, Container, ContainerWriteGuard, CopyError, EntryKey, LinkInfo, LinkToError,
    RemoveError, RenameError, UnlinkToError, WriteError,
};

use super::super::metadata::{ConfigRule, from_rules, rules_path, save_rules};
use super::super::transaction::{reopen_target, resolve_target};
use super::ConfigurableContainerWriteGuard;

impl ConfigurableContainerWriteGuard {
    /// Replaces ordered rules after validating every locator and target UID.
    ///
    /// # Arguments
    ///
    /// * `rules` - Complete first-match-wins replacement rule list.
    ///
    /// # Returns
    ///
    /// `Ok(())` after synchronized persistence under the current source lock.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid patterns, unresolved/mismatched/self targets,
    /// a released writer, or metadata persistence failures.
    pub fn set_rules(&mut self, rules: Vec<ConfigRule>) -> Result<()> {
        let source_uid = self.source()?.container_uid().to_owned();
        for rule in &rules {
            let target = resolve_target(&self.root, &rule.container.locator, &rule.container.uid)?;
            if target.uid == source_uid {
                bail!("a ConfigurableContainer rule cannot target itself");
            }
        }
        save_rules(&rules_path(&self.root), &from_rules(rules))
    }

    /// Sets the default relative-path policy used by automatic and explicit links.
    ///
    /// # Arguments
    ///
    /// * `prefer_relative` - `true` to prefer relative recorded paths, `false` for absolute paths.
    ///
    /// # Returns
    ///
    /// `Ok(())` after outgoing metadata is updated.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer was released, metadata cannot be persisted,
    /// or existing outgoing links would be reinterpreted.
    pub fn set_prefer_relative(&mut self, prefer_relative: bool) -> Result<()> {
        self.source_mut()?.set_prefer_relative(prefer_relative)
    }

    /// Adds an ordinary local entry, bypassing automatic rules.
    ///
    /// # Arguments
    ///
    /// * `key` - New local entry key.
    /// * `data` - Complete bytes to store locally.
    ///
    /// # Returns
    ///
    /// `Ok(())` after local storage is installed.
    ///
    /// # Errors
    ///
    /// Returns [`AddError`] for invalid, occupied, linked, or persistence failures.
    pub fn local_add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        self.source_mut().map_err(AddError::Other)?.add(key, data)
    }

    /// Writes an ordinary local entry, bypassing automatic rules.
    ///
    /// # Arguments
    ///
    /// * `key` - Local entry key to create or replace.
    /// * `data` - Complete replacement bytes.
    ///
    /// # Returns
    ///
    /// `Ok(())` after local storage is updated.
    ///
    /// # Errors
    ///
    /// Returns [`WriteError`] for invalid/outgoing keys or persistence failures.
    pub fn local_write(
        &mut self,
        key: &EntryKey,
        data: Buffer,
    ) -> std::result::Result<(), WriteError> {
        self.source_mut()
            .map_err(WriteError::Other)?
            .write(key, data)
    }

    /// Removes an ordinary local entry, bypassing automatic rules.
    ///
    /// # Arguments
    ///
    /// * `key` - Local entry key to remove.
    ///
    /// # Returns
    ///
    /// `Ok(())` after deletion.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveError`] for missing, invalid, or protected entries.
    pub fn local_remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError> {
        self.source_mut().map_err(RemoveError::Other)?.remove(key)
    }

    /// Renames an ordinary local entry, bypassing automatic rules.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing local key.
    /// * `to` - Unoccupied local destination key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after rename.
    ///
    /// # Errors
    ///
    /// Returns [`RenameError`] for missing, occupied, invalid, or linked entries.
    pub fn local_rename(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), RenameError> {
        self.source_mut()
            .map_err(RenameError::Other)?
            .rename(from, to)
    }

    /// Copies an ordinary local entry, bypassing automatic rules.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing local source key.
    /// * `to` - Unoccupied local destination key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after copying bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CopyError`] for missing, occupied, invalid, or linked entries.
    pub fn local_copy(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), CopyError> {
        self.source_mut().map_err(CopyError::Other)?.copy(from, to)
    }

    /// Creates one explicit outgoing link, bypassing automatic routing.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - New outgoing key.
    /// * `target` - Target Container handle.
    /// * `target_key` - Existing ordinary target key.
    /// * `prefer_relative` - Optional path-policy override.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal link installation.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for conflicts, unavailable/broken targets, or persistence failures.
    pub fn link_to(
        &mut self,
        linker_key: &EntryKey,
        target: &mut dyn Container,
        target_key: &EntryKey,
        prefer_relative: Option<bool>,
    ) -> std::result::Result<(), LinkToError> {
        let participant = reopen_target(target).map_err(LinkToError::Other)?;
        let target_uid = participant.uid.clone();
        let mut transaction = self
            .transaction(vec![participant])
            .map_err(LinkToError::Other)?;
        let result = (|| {
            let (source, target) = transaction
                .source_and_target(&target_uid)
                .map_err(LinkToError::Other)?;
            source.link_to_locked(
                linker_key,
                &target.root,
                target.guard.as_mut(),
                target_key,
                prefer_relative,
            )
        })();
        self.restore_source(transaction);
        result
    }

    /// Removes one explicit outgoing link.
    ///
    /// # Arguments
    ///
    /// * `key` - Existing outgoing key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal unlink.
    ///
    /// # Errors
    ///
    /// Returns [`UnlinkToError`] for absent, broken, unavailable, or persistence failures.
    pub fn unlink_to(&mut self, key: &EntryKey) -> std::result::Result<(), UnlinkToError> {
        let info = self
            .source()
            .map_err(UnlinkToError::Other)?
            .link_info(key)?;
        if !matches!(info, LinkInfo::LinkTo { .. }) {
            return Err(UnlinkToError::LinkNotFound);
        }
        self.remove_automatic(key).map_err(UnlinkToError::Other)
    }

    /// Copies one explicit outgoing relationship.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key.
    /// * `to` - New outgoing key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after an additional reciprocal relationship exists.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for broken, unavailable, conflicting, or persistence failures.
    pub fn link_copy(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), LinkToError> {
        if !matches!(
            self.source().map_err(LinkToError::Other)?.link_info(from)?,
            LinkInfo::LinkTo { .. }
        ) {
            return Err(LinkToError::TargetNotFound(from.clone()));
        }
        self.preserve_property(from, to, true)
            .map_err(LinkToError::Other)
    }

    /// Renames one explicit outgoing relationship.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key.
    /// * `to` - New outgoing key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal and local key rename.
    ///
    /// # Errors
    ///
    /// Returns an error for broken, unavailable, conflicting, or persistence failures.
    pub fn link_rename(&mut self, from: &EntryKey, to: &EntryKey) -> Result<()> {
        if !matches!(self.source()?.link_info(from)?, LinkInfo::LinkTo { .. }) {
            bail!("outgoing link does not exist: {from}");
        }
        self.preserve_property(from, to, false)
    }
}
