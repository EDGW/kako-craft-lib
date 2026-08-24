use super::*;
use crate::container::{ContainerLinkSnapshot, IncomingLinkRecord, LinkSource};

pub struct LocalContainerWriteGuard {
    pub(super) root: PathBuf,
    pub(super) links_path: PathBuf,
    pub(super) logger: ContainerLogger,
    // Keeping the descriptor alive keeps the OS-level advisory lock held.
    // The lock is automatically released on Drop and on process termination.
    pub(super) _lock_file: File,
}

impl LocalContainerWriteGuard {
    fn entry_path(&self, key: &EntryKey) -> std::result::Result<PathBuf, EntryKey> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    fn links(&self) -> Result<IncomingLinksMetadata> {
        if !self.links_path.exists() {
            return Ok(IncomingLinksMetadata::default());
        }
        let links = read_incoming_metadata(&self.links_path)?;
        let actual_count = links.links.values().map(Vec::len).sum::<usize>() as u64;
        if links.count != actual_count {
            return Err(anyhow!(
                "invalid link count in {}: recorded {}, actual {}",
                self.links_path.display(),
                links.count,
                actual_count
            ));
        }
        Ok(links)
    }

    fn save_links(&self, links: &mut IncomingLinksMetadata) -> Result<()> {
        links.count = links.links.values().map(Vec::len).sum::<usize>() as u64;
        #[cfg(test)]
        if should_fail_incoming_metadata_write() {
            return Err(anyhow!("injected incoming metadata write failure"));
        }
        write_json_atomic(&self.links_path, links)
    }

    fn is_linked_from(&self, key: &EntryKey) -> Result<bool> {
        Ok(self.links()?.links.contains_key(key))
    }
}

impl ContainerWriteGuard for LocalContainerWriteGuard {
    fn container_uid(&self) -> &str {
        self.logger.uid()
    }

    fn link_snapshot(&self) -> Result<ContainerLinkSnapshot> {
        let links = self.links()?;
        let incoming = links
            .links
            .into_iter()
            .flat_map(|(target_key, sources)| {
                sources.into_iter().map(move |source| IncomingLinkRecord {
                    target_key: target_key.clone(),
                    linker_uid: source.linker_uid,
                    linker_key: source.linker_key,
                })
            })
            .collect();
        Ok(ContainerLinkSnapshot {
            container_uid: self.logger.uid().to_owned(),
            container_path: self.root.clone(),
            incoming,
            outgoing: Vec::new(),
        })
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.entry_path(key)
            .map_err(|key| anyhow!("invalid entry key: {key}"))
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let path = self.filepath(key)?;
        fs::read(&path).with_context(|| format!("failed to read entry {}", path.display()))
    }

    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        let span = container_operation_span!(self.logger, "add");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "adding container entry");
        let path = self.entry_path(key).map_err(AddError::InvalidEntryKey)?;
        if self.is_linked_from(key).map_err(AddError::Other)? {
            return Err(AddError::EntryExists(key.clone()));
        }
        match write_entry_atomic(&self.root, &path, &data, false) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(AddError::EntryExists(key.clone()));
            }
            Err(error) => return Err(AddError::Other(error.into())),
        }
        info!(entry_key = %key, byte_count = data.len(), "added container entry");
        Ok(())
    }

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError> {
        let span = container_operation_span!(self.logger, "write");
        let _entered = span.enter();
        debug!(entry_key = %key, byte_count = data.len(), "writing container entry");
        let path = self.entry_path(key).map_err(WriteError::InvalidEntryKey)?;
        let byte_count = data.len();
        write_entry_atomic(&self.root, &path, &data, true)
            .map_err(|error| WriteError::Other(error.into()))?;
        info!(entry_key = %key, byte_count, "wrote container entry");
        Ok(())
    }

    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError> {
        let span = container_operation_span!(self.logger, "remove");
        let _entered = span.enter();
        debug!(entry_key = %key, "removing container entry");
        let path = self.entry_path(key).map_err(RemoveError::InvalidEntryKey)?;
        if self.is_linked_from(key).map_err(RemoveError::Other)? {
            return Err(RemoveError::EntryIsLinked(key.clone()));
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                remove_empty_parents(path.parent(), &self.root);
                info!(entry_key = %key, "removed container entry");
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(RemoveError::EntryNotFound(key.clone()))
            }
            Err(error) => Err(RemoveError::Other(error.into())),
        }
    }

    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError> {
        let span = container_operation_span!(self.logger, "rename");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "renaming container entry");
        let from_path = self
            .entry_path(from)
            .map_err(RenameError::InvalidEntryKey)?;
        let to_path = self.entry_path(to).map_err(RenameError::InvalidEntryKey)?;
        if self.is_linked_from(from).map_err(RenameError::Other)? {
            return Err(RenameError::EntryIsLinked(from.clone()));
        }
        if self.is_linked_from(to).map_err(RenameError::Other)? {
            return Err(RenameError::EntryIsLinked(to.clone()));
        }
        if !from_path.is_file() {
            return Err(RenameError::EntryNotFound(from.clone()));
        }
        if fs::symlink_metadata(&to_path).is_ok() {
            return Err(RenameError::EntryExists(to.clone()));
        }
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).map_err(|error| RenameError::Other(error.into()))?;
        }
        fs::rename(&from_path, &to_path).map_err(|error| RenameError::Other(error.into()))?;
        remove_empty_parents(from_path.parent(), &self.root);
        info!(from = %from, to = %to, "renamed container entry");
        Ok(())
    }

    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError> {
        let span = container_operation_span!(self.logger, "copy");
        let _entered = span.enter();
        debug!(from = %from, to = %to, "copying container entry");
        let from_path = self.entry_path(from).map_err(CopyError::InvalidEntryKey)?;
        let to_path = self.entry_path(to).map_err(CopyError::InvalidEntryKey)?;
        if !from_path.is_file() {
            return Err(CopyError::EntryNotFound(from.clone()));
        }
        if self.is_linked_from(to).map_err(CopyError::Other)?
            || fs::symlink_metadata(&to_path).is_ok()
        {
            return Err(CopyError::EntryExists(to.clone()));
        }
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).map_err(|error| CopyError::Other(error.into()))?;
        }
        fs::copy(&from_path, &to_path).map_err(|error| CopyError::Other(error.into()))?;
        info!(from = %from, to = %to, "copied container entry");
        Ok(())
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
        let target_path = self
            .entry_path(target_key)
            .map_err(LinkFromError::InvalidEntryKey)?;
        validate_key(linker_key).map_err(LinkFromError::InvalidEntryKey)?;
        if !target_path.is_file() {
            return Err(LinkFromError::TargetNotFound(target_key.clone()));
        }

        let mut links = self.links().map_err(LinkFromError::Other)?;
        let incoming = links.links.entry(target_key.clone()).or_default();
        if incoming
            .iter()
            .any(|link| link.linker_uid == linker_uid && link.linker_key == *linker_key)
        {
            return Err(LinkFromError::AlreadyLinked);
        }
        incoming.push(IncomingLink {
            linker_key: linker_key.clone(),
            linker_uid: linker_uid.to_owned(),
        });
        self.save_links(&mut links).map_err(LinkFromError::Other)?;
        info!(
            target_key = %target_key,
            linker_uid,
            linker_key = %linker_key,
            "registered incoming link"
        );
        Ok(())
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
        validate_key(target_key).map_err(UnlinkError::InvalidEntryKey)?;
        validate_key(linker_key).map_err(UnlinkError::InvalidEntryKey)?;
        let mut links = self.links().map_err(UnlinkError::Other)?;
        let Some(incoming) = links.links.get_mut(target_key) else {
            return Err(UnlinkError::LinkNotFound);
        };
        let Some(position) = incoming.iter().position(|existing| {
            existing.linker_uid == linker_uid && existing.linker_key == *linker_key
        }) else {
            return Err(UnlinkError::LinkMismatch);
        };
        incoming.remove(position);
        if incoming.is_empty() {
            links.links.remove(target_key);
        }
        self.save_links(&mut links).map_err(UnlinkError::Other)?;
        info!(
            target_key = %target_key,
            linker_uid,
            linker_key = %linker_key,
            "removed incoming link"
        );
        Ok(())
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
        debug!(
            target_key = %target_key,
            linker_uid,
            from = %from,
            to = %to,
            "renaming incoming link"
        );
        validate_key(target_key).map_err(LinkFromError::InvalidEntryKey)?;
        validate_key(from).map_err(LinkFromError::InvalidEntryKey)?;
        validate_key(to).map_err(LinkFromError::InvalidEntryKey)?;
        if from == to {
            return Ok(());
        }

        let mut links = self.links().map_err(LinkFromError::Other)?;
        let Some(incoming) = links.links.get_mut(target_key) else {
            return Err(LinkFromError::LinkConflict);
        };
        if incoming
            .iter()
            .any(|existing| existing.linker_uid == linker_uid && existing.linker_key == *to)
        {
            return Err(LinkFromError::AlreadyLinked);
        }
        let positions = incoming
            .iter()
            .enumerate()
            .filter_map(|(index, existing)| {
                (existing.linker_uid == linker_uid && existing.linker_key == *from).then_some(index)
            })
            .collect::<Vec<_>>();
        let [position] = positions.as_slice() else {
            return Err(LinkFromError::LinkConflict);
        };
        incoming[*position].linker_key = to.clone();
        self.save_links(&mut links).map_err(LinkFromError::Other)?;
        info!(
            target_key = %target_key,
            linker_uid,
            from = %from,
            to = %to,
            "renamed incoming link"
        );
        Ok(())
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        debug!(entry_key = %key, "getting link information");
        validate_key(key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        let links = self.links()?;
        Ok(match links.links.get(key) {
            Some(links) => {
                let mut linkers = links
                    .iter()
                    .map(|link| LinkSource {
                        linker_key: link.linker_key.clone(),
                        linker_uid: link.linker_uid.clone(),
                    })
                    .collect::<Vec<_>>();
                linkers.sort_by(|left, right| {
                    left.linker_uid
                        .cmp(&right.linker_uid)
                        .then_with(|| left.linker_key.cmp(&right.linker_key))
                });
                LinkInfo::LinkFrom { linkers }
            }
            None => LinkInfo::None,
        })
    }

    fn has_link_from(
        &self,
        target_key: &EntryKey,
        linker_uid: &str,
        linker_key: &EntryKey,
    ) -> Result<bool> {
        validate_key(target_key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        Ok(self.links()?.links.get(target_key).is_some_and(|links| {
            links
                .iter()
                .any(|link| link.linker_uid == linker_uid && link.linker_key == *linker_key)
        }))
    }
}

impl Drop for LocalContainerWriteGuard {
    fn drop(&mut self) {
        trace!(
            logger = %self.logger.name(),
            container_uid = %self.logger.uid(),
            container_kind = self.logger.kind(),
            operation = "writer",
            path = %self.root.join(CONTROL_DIR).join(LOCK_FILE).display(),
            "releasing file lock"
        );
    }
}
