//! Automatic first-match routing and replace transactions.

use std::fs;

use anyhow::{Result, anyhow, bail};
use tracing::info;

use crate::container::{Buffer, ContainerWriteGuard, EntryKey, LinkContainerWriteGuard, LinkInfo};

use super::super::metadata::{load_rules, rules_path};
use super::super::routing::{DataRoute, route_data};
use super::super::transaction::{
    MultiContainerWriteTransaction, resolve_recorded_target, resolve_target,
};
use super::ConfigurableContainerWriteGuard;

impl ConfigurableContainerWriteGuard {
    /// Performs automatic add according to one already calculated route.
    ///
    /// # Arguments
    ///
    /// * `key` - New source entry key.
    /// * `data` - Complete entry data.
    /// * `route` - First-match routing result.
    ///
    /// # Returns
    ///
    /// `Ok(())` after local or linked installation.
    ///
    /// # Errors
    ///
    /// Returns an error for occupied keys, target resolution/locking, UID mismatch,
    /// target conflicts, or link persistence failures.
    pub(super) fn add_routed(
        &mut self,
        key: &EntryKey,
        data: Buffer,
        route: DataRoute,
    ) -> Result<()> {
        match route {
            DataRoute::Local => {
                info!(entry_key = %key, storage = "local", "selected configurable route");
                self.source_mut()?.add(key, data)?;
                Ok(())
            }
            DataRoute::Link {
                pattern,
                locator,
                container_uid,
                target_key,
            } => {
                let target = resolve_target(&self.root, &locator, &container_uid)?;
                let expected_route = DataRoute::Link {
                    pattern: pattern.clone(),
                    locator: locator.clone(),
                    container_uid: container_uid.clone(),
                    target_key: target_key.clone(),
                };
                let route_data_copy = data.clone();
                let mut transaction = self.transaction(vec![target])?;
                let result = (|| -> Result<()> {
                    let current_rules = load_rules(&rules_path(&self.root))?;
                    if route_data(&self.root, &current_rules, key, &route_data_copy)?
                        != expected_route
                    {
                        bail!("configurable rules changed during transaction preflight");
                    }
                    ensure_source_absent(transaction.source(), key)?;
                    let created = {
                        let target = transaction.target(&container_uid)?;
                        ensure_target_entry(target.guard.as_mut(), &target_key, data)?
                    };
                    let (source, target) = transaction.source_and_target(&container_uid)?;
                    if let Err(error) = source.link_to_locked(
                        key,
                        &target.root,
                        target.guard.as_mut(),
                        &target_key,
                        None,
                    ) {
                        if created {
                            let _ = target.guard.remove(&target_key);
                        }
                        return Err(error.into());
                    }
                    info!(entry_key = %key, %pattern, target_locator = %locator, %target_key, reused = !created, storage = "link", "selected configurable route");
                    Ok(())
                })();
                self.restore_source(transaction);
                result
            }
        }
    }

    /// Performs replace-as-remove-and-add under one complete lock set.
    ///
    /// # Arguments
    ///
    /// * `key` - Existing or new source key.
    /// * `data` - Complete replacement bytes.
    /// * `route` - Current automatic route.
    ///
    /// # Returns
    ///
    /// `Ok(())` after replacement or a validated no-op fast path.
    ///
    /// # Errors
    ///
    /// Returns an error for broken old links, protected local targets, contention,
    /// conflicts, mutation failures, or incomplete rollback.
    pub(super) fn write_routed(
        &mut self,
        key: &EntryKey,
        data: Buffer,
        route: DataRoute,
    ) -> Result<()> {
        let old = self.source()?.link_info(key)?;
        let expected_route = route.clone();
        let route_data_copy = data.clone();
        let old_data =
            if matches!(old, LinkInfo::None) && self.source()?.entry_filepath(key)?.is_file() {
                Some(self.source()?.read(key)?)
            } else {
                None
            };
        let mut targets = Vec::new();
        let mut old_target_root = None;
        let mut fast_target_uid = None;
        if let LinkInfo::LinkTo {
            container_uid,
            container_path,
            ..
        } = &old
        {
            let target = resolve_recorded_target(&self.root, container_path, container_uid)?;
            old_target_root = Some(target.root.clone());
            targets.push(target);
        }
        if let DataRoute::Link {
            locator,
            container_uid,
            target_key,
            ..
        } = &route
        {
            let target = resolve_target(&self.root, locator, container_uid)?;
            if let LinkInfo::LinkTo {
                target_key: old_key,
                container_uid: old_uid,
                ..
            } = &old
                && old_uid == container_uid
                && old_key == target_key
                && old_target_root.as_ref() == Some(&target.root)
            {
                fast_target_uid = Some(container_uid.clone());
            }
            targets.push(target);
        }
        let mut transaction = self.transaction(targets)?;
        let result = (|| -> Result<()> {
            let current_rules = load_rules(&rules_path(&self.root))?;
            if route_data(&self.root, &current_rules, key, &route_data_copy)? != expected_route {
                bail!("configurable rules changed during transaction preflight");
            }
            let current = transaction.source().link_info(key)?;
            if current != old {
                bail!("configurable entry changed during transaction preflight");
            }
            if let Some(container_uid) = &fast_target_uid {
                let (source, target) = transaction.source_and_target(container_uid)?;
                source.validate_outgoing_locked(key, target.guard.as_ref())?;
                info!(entry_key = %key, "reused unchanged configurable link");
                return Ok(());
            }
            remove_old(&mut transaction, key, &old)?;
            if let Err(error) = add_new(&mut transaction, key, data, &route) {
                if let Err(rollback) = restore_old(&mut transaction, key, &old, old_data) {
                    return Err(anyhow!(
                        "configurable write failed: {error:#}; rollback failed: {rollback:#}"
                    ));
                }
                return Err(error);
            }
            log_route(key, &route);
            Ok(())
        })();
        self.restore_source(transaction);
        result
    }

    /// Removes an automatic entry while preserving content-addressed targets.
    ///
    /// # Arguments
    ///
    /// * `key` - Existing local or outgoing key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after local deletion or reciprocal unlink.
    ///
    /// # Errors
    ///
    /// Returns an error for missing/protected local entries or broken/unavailable links.
    pub(super) fn remove_automatic(&mut self, key: &EntryKey) -> Result<()> {
        let info = self.source()?.link_info(key)?;
        let LinkInfo::LinkTo {
            container_uid,
            container_path,
            ..
        } = info
        else {
            self.source_mut()?.remove(key)?;
            return Ok(());
        };
        let target = resolve_recorded_target(&self.root, &container_path, &container_uid)?;
        let mut transaction = self.transaction(vec![target])?;
        let result = (|| -> Result<()> {
            let (source, target) = transaction.source_and_target(&container_uid)?;
            source.unlink_to_locked(key, target.guard.as_mut())?;
            Ok(())
        })();
        self.restore_source(transaction);
        result
    }

    /// Renames or copies while preserving the source storage property.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing source key.
    /// * `to` - Unoccupied destination key.
    /// * `copy` - `true` to copy, `false` to rename.
    ///
    /// # Returns
    ///
    /// `Ok(())` after a local or outgoing operation.
    ///
    /// # Errors
    ///
    /// Returns an error for source/target state, contention, validation, or persistence failures.
    pub(super) fn preserve_property(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
        copy: bool,
    ) -> Result<()> {
        let info = self.source()?.link_info(from)?;
        let LinkInfo::LinkTo {
            container_uid,
            container_path,
            ..
        } = info
        else {
            if copy {
                self.source_mut()?.copy(from, to)?;
            } else {
                self.source_mut()?.rename(from, to)?;
            }
            return Ok(());
        };
        let target = resolve_recorded_target(&self.root, &container_path, &container_uid)?;
        let mut transaction = self.transaction(vec![target])?;
        let result = (|| -> Result<()> {
            let (source, target) = transaction.source_and_target(&container_uid)?;
            if copy {
                source.link_copy_locked(from, to, target.guard.as_mut())?;
            } else {
                source.link_rename_locked(from, to, target.guard.as_mut())?;
            }
            Ok(())
        })();
        self.restore_source(transaction);
        result
    }
}

/// Ensures a source key is absent from metadata and filesystem state.
///
/// # Arguments
///
/// * `source` - Locked configurable source whose raw key state is inspected.
/// * `key` - Prospective new entry key.
///
/// # Returns
///
/// `Ok(())` only when no local file or incoming/outgoing record owns `key`.
///
/// # Errors
///
/// Returns an error for occupied state or metadata/path failures.
fn ensure_source_absent(source: &LinkContainerWriteGuard, key: &EntryKey) -> Result<()> {
    if !matches!(source.link_info(key)?, LinkInfo::None)
        || fs::symlink_metadata(source.entry_filepath(key)?).is_ok()
    {
        bail!("entry already exists: {key}");
    }
    Ok(())
}

/// Ensures a content-addressed target is an ordinary file, creating it when absent.
///
/// # Arguments
///
/// * `target` - Locked target Container writer.
/// * `key` - Two-level SHA-1 target entry key.
/// * `data` - Bytes written only when the target is absent.
///
/// # Returns
///
/// `true` when this call created the target; `false` when it reused an ordinary file.
///
/// # Errors
///
/// Returns an error when the key is outgoing, its path is not an ordinary file,
/// metadata cannot be inspected, or adding data fails.
fn ensure_target_entry(
    target: &mut dyn ContainerWriteGuard,
    key: &EntryKey,
    data: Buffer,
) -> Result<bool> {
    if matches!(target.link_info(key)?, LinkInfo::LinkTo { .. }) {
        bail!("SHA-1 target is an outgoing link: {key}");
    }
    let path = target.entry_filepath(key)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(false)
        }
        Ok(_) => bail!("SHA-1 target is not an ordinary file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            target.add(key, data)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

/// Removes preflighted old state under the complete lock set.
///
/// # Arguments
///
/// * `transaction` - Transaction owning the source and all referenced targets.
/// * `key` - Source key being replaced.
/// * `old` - Link classification captured before lock reacquisition and revalidated afterward.
///
/// # Returns
///
/// `Ok(())` after local removal, reciprocal unlink, or an absent-key no-op.
///
/// # Errors
///
/// Returns an error for protected local entries or broken/missing target state.
fn remove_old(
    transaction: &mut MultiContainerWriteTransaction,
    key: &EntryKey,
    old: &LinkInfo,
) -> Result<()> {
    match old {
        LinkInfo::LinkTo { container_uid, .. } => {
            let (source, target) = transaction.source_and_target(container_uid)?;
            source.unlink_to_locked(key, target.guard.as_mut())?;
        }
        LinkInfo::LinkFrom { .. } => transaction.source().remove(key)?,
        LinkInfo::None => {
            if transaction.source().entry_filepath(key)?.is_file() {
                transaction.source().remove(key)?;
            }
        }
    }
    Ok(())
}

/// Installs replacement state under the complete lock set.
///
/// # Arguments
///
/// * `transaction` - Transaction owning source and selected target writers.
/// * `key` - Source entry key to install.
/// * `data` - Complete replacement bytes.
/// * `route` - Revalidated local or SHA-1 link route.
///
/// # Returns
///
/// `Ok(())` after local or linked installation.
///
/// # Errors
///
/// Returns an error for target conflicts, link failures, or local persistence failures.
fn add_new(
    transaction: &mut MultiContainerWriteTransaction,
    key: &EntryKey,
    data: Buffer,
    route: &DataRoute,
) -> Result<()> {
    match route {
        DataRoute::Local => transaction.source().add(key, data).map_err(Into::into),
        DataRoute::Link {
            container_uid,
            target_key,
            ..
        } => {
            let created = {
                let target = transaction.target(container_uid)?;
                ensure_target_entry(target.guard.as_mut(), target_key, data)?
            };
            let (source, target) = transaction.source_and_target(container_uid)?;
            if let Err(error) =
                source.link_to_locked(key, &target.root, target.guard.as_mut(), target_key, None)
            {
                if created {
                    let _ = target.guard.remove(target_key);
                }
                return Err(error.into());
            }
            Ok(())
        }
    }
}

/// Best-effort restores preflighted old state after replacement failure.
///
/// # Arguments
///
/// * `transaction` - Still-locked source and targets.
/// * `key` - Source key to restore.
/// * `old` - Original local/incoming/outgoing classification.
/// * `old_data` - Original local bytes when an ordinary file existed.
///
/// # Returns
///
/// `Ok(())` when old state is restored or was originally absent.
///
/// # Errors
///
/// Returns an error when local or reciprocal outgoing restoration fails.
fn restore_old(
    transaction: &mut MultiContainerWriteTransaction,
    key: &EntryKey,
    old: &LinkInfo,
    old_data: Option<Buffer>,
) -> Result<()> {
    match old {
        LinkInfo::LinkTo {
            target_key,
            container_uid,
            container_path,
        } => {
            let (source, target) = transaction.source_and_target(container_uid)?;
            source.link_to_locked(
                key,
                &target.root,
                target.guard.as_mut(),
                target_key,
                Some(container_path.is_relative()),
            )?;
        }
        LinkInfo::None => {
            if let Some(data) = old_data {
                transaction.source().add(key, data)?;
            }
        }
        LinkInfo::LinkFrom { .. } => {
            bail!("cannot restore a protected local target after removal")
        }
    }
    Ok(())
}

/// Emits the structured INFO decision for a successful automatic write.
///
/// # Arguments
///
/// * `key` - Written source entry key.
/// * `route` - Route whose mutation completed.
fn log_route(key: &EntryKey, route: &DataRoute) {
    match route {
        DataRoute::Local => {
            info!(entry_key = %key, storage = "local", "selected configurable route")
        }
        DataRoute::Link {
            pattern,
            locator,
            target_key,
            ..
        } => {
            info!(entry_key = %key, %pattern, target_locator = %locator, %target_key, storage = "link", "selected configurable route")
        }
    }
}
