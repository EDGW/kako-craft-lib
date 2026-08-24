use super::*;
use crate::container::{
    ContainerLinkSnapshot, OutgoingLinkRecord, apply_validation_check_action,
    validation_check_issues,
};

impl ContainerWriteGuard for LinkContainerWriteGuard {
    fn container_uid(&self) -> &str {
        &self.uid
    }

    fn link_snapshot(&self) -> Result<ContainerLinkSnapshot> {
        let mut snapshot = self.local.link_snapshot()?;
        let metadata = self.outgoing_metadata()?;
        snapshot.container_uid = self.uid.clone();
        snapshot.container_path = self.root.clone();
        snapshot.outgoing = metadata
            .links
            .into_iter()
            .map(|(linker_key, link)| OutgoingLinkRecord {
                linker_key,
                target_container_uid: link.container_uid,
                target_container_path: link.container_path,
                target_key: link.target_key,
            })
            .collect();
        Ok(snapshot)
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.filepath_verified(key)
    }

    fn entry_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.entry_path(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        self.read_verified(key)
    }

    fn check(&mut self, corresponding: &[&dyn Container]) -> Result<Vec<LinkCheckIssue>> {
        let validation = validation_check_issues(self, corresponding)?;
        let blocked_symlink_keys = validation
            .iter()
            .filter(|issue| {
                matches!(
                    issue.kind,
                    LinkCheckKind::Validation(_) | LinkCheckKind::Unavailable
                )
            })
            .map(|issue| issue.key.clone())
            .collect::<BTreeSet<_>>();
        let mut issues = self.check_links()?;
        issues.retain(|issue| {
            issue.kind == LinkCheckKind::UnrecordedSymlink
                || !blocked_symlink_keys.contains(&issue.key)
        });
        issues.extend(validation);
        issues.sort_by(|left, right| left.id.cmp(&right.id));
        issues.dedup_by(|left, right| left.id == right.id);
        Ok(issues)
    }

    fn apply_check_action(
        &mut self,
        issue: &LinkCheckIssue,
        action: CheckRepairAction,
        corresponding: &[&dyn Container],
    ) -> Result<CheckActionResult> {
        match action {
            CheckRepairAction::CreateMissingSymlink
            | CheckRepairAction::ReplaceIncorrectSymlink
            | CheckRepairAction::DeleteUnrecordedSymlink => self.repair_link_issue(issue, action),
            CheckRepairAction::RemoveLocalOutgoingOnly => {
                self.remove_outgoing_link(&issue.key)?;
                warn!(entry_key = %issue.key, "removed local outgoing link without changing reciprocal metadata");
                Ok(CheckActionResult {
                    description: format!(
                        "removed local outgoing record and symlink {} only",
                        issue.key
                    ),
                })
            }
            _ => apply_validation_check_action(self, issue, action, corresponding),
        }
    }

    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        let span = container_operation_span!(self.logger, "add");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "adding local container entry");
        if self
            .outgoing_metadata()
            .map_err(AddError::Other)?
            .links
            .contains_key(key)
        {
            return Err(AddError::EntryIsLink(key.clone()));
        }
        self.local.add(key, data)
    }

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError> {
        let span = container_operation_span!(self.logger, "write");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "writing container entry");
        if self
            .outgoing_metadata()
            .map_err(WriteError::Other)?
            .links
            .contains_key(key)
        {
            return Err(WriteError::EntryIsLink(key.clone()));
        }
        self.local.write(key, data)
    }

    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError> {
        let span = container_operation_span!(self.logger, "remove");
        let _entered = span.enter();
        debug!(entry_key = %key, "removing local container entry");
        if self
            .outgoing_metadata()
            .map_err(RemoveError::Other)?
            .links
            .contains_key(key)
        {
            return Err(RemoveError::EntryIsLinked(key.clone()));
        }
        self.local.remove(key)
    }

    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError> {
        let span = container_operation_span!(self.logger, "rename");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "renaming local container entry");
        let metadata = self.outgoing_metadata().map_err(RenameError::Other)?;
        if metadata.links.contains_key(from) {
            return Err(RenameError::EntryIsLinked(from.clone()));
        }
        if metadata.links.contains_key(to) {
            return Err(RenameError::EntryIsLinked(to.clone()));
        }
        self.local.rename(from, to)
    }

    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError> {
        let span = container_operation_span!(self.logger, "copy");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "copying local container entry");
        let metadata = self.outgoing_metadata().map_err(CopyError::Other)?;
        if metadata.links.contains_key(from) {
            return Err(CopyError::EntryIsLink(from.clone()));
        }
        if metadata.links.contains_key(to) {
            return Err(CopyError::EntryExists(to.clone()));
        }
        self.local.copy(from, to)
    }

    fn link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError> {
        let span = container_operation_span!(self.logger, "link_from");
        let _entered = span.enter();
        debug!(target_key = %target_key, linker_key = %linker_key, "registering incoming link");
        if self
            .outgoing_metadata()
            .map_err(LinkFromError::Other)?
            .links
            .contains_key(target_key)
        {
            return Err(LinkFromError::LinkConflict);
        }
        self.local.link_from(linker_uid, target_key, linker_key)
    }

    fn unlink(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError> {
        let span = container_operation_span!(self.logger, "unlink");
        let _entered = span.enter();
        debug!(target_key = %target_key, linker_key = %linker_key, "removing incoming link");
        self.local.unlink(linker_uid, target_key, linker_key)
    }

    fn rename_link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), LinkFromError> {
        let span = container_operation_span!(self.logger, "rename_link_from");
        let _entered = span.enter();
        debug!(target_key = %target_key, from = %from, to = %to, "renaming incoming link");
        if self
            .outgoing_metadata()
            .map_err(LinkFromError::Other)?
            .links
            .contains_key(target_key)
        {
            return Err(LinkFromError::LinkConflict);
        }
        self.local
            .rename_link_from(linker_uid, target_key, from, to)
    }

    fn repair_add_outgoing(
        &mut self,
        linker_key: &EntryKey,
        target_container_uid: &str,
        target_container_path: &Path,
        target_key: &EntryKey,
    ) -> Result<()> {
        let target_root = absolute_path(target_container_path)?;
        let actual_uid = open_container(&target_root)?.uid()?;
        if actual_uid != target_container_uid {
            bail!(
                "target container UID mismatch at {}: expected {}, found {}",
                target_root.display(),
                target_container_uid,
                actual_uid
            );
        }
        let preference = self.outgoing_metadata()?.prefer_relative;
        let recorded_path =
            recorded_container_path(&absolute_path(&self.root)?, &target_root, preference);
        self.install_outgoing_link(
            linker_key,
            target_key,
            target_container_uid.to_owned(),
            recorded_path,
        )?;
        info!(linker_key = %linker_key, target_key = %target_key, "restored missing outgoing link");
        Ok(())
    }

    fn repair_remove_outgoing(&mut self, linker_key: &EntryKey) -> Result<()> {
        self.remove_outgoing_link(linker_key)?;
        info!(linker_key = %linker_key, "removed stale outgoing link");
        Ok(())
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        trace!(entry_key = %key, "getting link information");
        let metadata = self.outgoing_metadata()?;
        if let Some(link) = metadata.links.get(key) {
            return Ok(LinkInfo::LinkTo {
                target_key: link.target_key.clone(),
                container_uid: link.container_uid.clone(),
                container_path: link.container_path.clone(),
            });
        }
        self.local.link_info(key)
    }

    fn has_link_from(
        &self,
        target_key: &EntryKey,
        linker_uid: &str,
        linker_key: &EntryKey,
    ) -> Result<bool> {
        self.local.has_link_from(target_key, linker_uid, linker_key)
    }
}
