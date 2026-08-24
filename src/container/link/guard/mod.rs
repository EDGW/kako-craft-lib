use super::*;

mod container;

pub struct LinkContainerWriteGuard {
    pub(super) root: PathBuf,
    pub(super) uid: String,
    pub(super) metadata_path: PathBuf,
    pub(super) local: Box<dyn ContainerWriteGuard>,
    pub(super) logger: ContainerLogger,
}

impl LinkContainerWriteGuard {
    pub fn set_prefer_relative(&mut self, prefer_relative: bool) -> Result<()> {
        let mut metadata = self.outgoing_metadata()?;
        if !metadata.links.is_empty() && metadata.prefer_relative != prefer_relative {
            bail!("cannot change prefer-relative while outgoing links exist");
        }
        metadata.prefer_relative = prefer_relative;
        self.save_metadata(&metadata)?;
        info!(prefer_relative, "updated link-container path preference");
        Ok(())
    }
    fn entry_path(&self, key: &EntryKey) -> Result<PathBuf> {
        super::super::local::validate_key(key)
            .map_err(|key| anyhow!("invalid entry key: {key}"))?;
        Ok(self.root.join(key))
    }

    pub(super) fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        read_metadata(&self.metadata_path)
    }

    fn save_metadata(&self, metadata: &OutgoingLinksMetadata) -> Result<()> {
        #[cfg(test)]
        if should_fail_outgoing_metadata_write() {
            bail!("injected outgoing metadata write failure");
        }
        write_json_atomic(&self.metadata_path, metadata)
    }

    fn filepath_verified(&self, key: &EntryKey) -> Result<PathBuf> {
        let link_path = self.entry_path(key)?;
        let metadata = self.outgoing_metadata()?;
        if let Some(link) = metadata.links.get(key) {
            let (_target_guard, _) =
                validate_outgoing_link_locked(&self.root, key, &self.uid, &link_path, link)
                    .map_err(LinkAccessError::into_anyhow)?;
        }
        Ok(link_path)
    }

    pub(super) fn local_filepath_verified(&self, key: &EntryKey) -> Result<PathBuf> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.filepath(key)
    }

    pub(super) fn local_read_verified(&self, key: &EntryKey) -> Result<Buffer> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.read(key)
    }

    fn read_verified(&self, key: &EntryKey) -> Result<Buffer> {
        let metadata = self.outgoing_metadata()?;
        let Some(link) = metadata.links.get(key) else {
            return self.local.read(key);
        };
        let link_path = self.entry_path(key)?;
        let (target_guard, _) =
            validate_outgoing_link_locked(&self.root, key, &self.uid, &link_path, link)
                .map_err(LinkAccessError::into_anyhow)?;
        target_guard.read(&link.target_key).map_err(|error| {
            BrokenLinkError::new(key, format!("failed to read target entry: {error:#}")).into()
        })
    }

    fn check_links(&self) -> Result<Vec<LinkCheckIssue>> {
        debug!("checking outgoing metadata and materialized symlinks");
        let metadata = self.outgoing_metadata()?;
        let root = self
            .metadata_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("link container has no root directory"))?;
        let mut issues = Vec::new();

        for (key, link) in &metadata.links {
            let path = root.join(key);
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    issues.push(LinkCheckIssue {
                        id: format!("symlink:missing:{key}"),
                        key: key.clone(),
                        kind: LinkCheckKind::MissingSymlink,
                        corresponding_container_uid: Some(link.container_uid.clone()),
                        corresponding_container_path: Some(resolved_container_path(root, link)),
                        linker_key: Some(key.clone()),
                        target_key: Some(link.target_key.clone()),
                        expected: Some(
                            symlink_target(root, &path, link, link.container_path.is_relative())
                                .display()
                                .to_string(),
                        ),
                        actual: None,
                        actions: vec![
                            CheckRepairAction::CreateMissingSymlink,
                            CheckRepairAction::Skip,
                        ],
                    });
                }
                Err(error) => return Err(error.into()),
                Ok(file_type) if !file_type.is_symlink() => {
                    issues.push(LinkCheckIssue {
                        id: format!("symlink:incorrect:{key}"),
                        key: key.clone(),
                        kind: LinkCheckKind::IncorrectSymlink,
                        corresponding_container_uid: Some(link.container_uid.clone()),
                        corresponding_container_path: Some(resolved_container_path(root, link)),
                        linker_key: Some(key.clone()),
                        target_key: Some(link.target_key.clone()),
                        expected: Some(
                            symlink_target(root, &path, link, link.container_path.is_relative())
                                .display()
                                .to_string(),
                        ),
                        actual: Some("filesystem entry is not a symlink".into()),
                        actions: vec![CheckRepairAction::Skip],
                    });
                }
                Ok(_) => {
                    let actual = fs::read_link(&path)?;
                    let expected =
                        symlink_target(root, &path, link, link.container_path.is_relative());
                    if actual != expected {
                        issues.push(LinkCheckIssue {
                            id: format!("symlink:incorrect:{key}"),
                            key: key.clone(),
                            kind: LinkCheckKind::IncorrectSymlink,
                            corresponding_container_uid: Some(link.container_uid.clone()),
                            corresponding_container_path: Some(resolved_container_path(root, link)),
                            linker_key: Some(key.clone()),
                            target_key: Some(link.target_key.clone()),
                            expected: Some(expected.display().to_string()),
                            actual: Some(actual.display().to_string()),
                            actions: vec![
                                CheckRepairAction::ReplaceIncorrectSymlink,
                                CheckRepairAction::Skip,
                            ],
                        });
                    }
                }
            }
        }

        let mut symlinks = BTreeMap::new();
        collect_symlinks(root, root, &mut symlinks)?;
        for (key, actual) in symlinks {
            if !metadata.links.contains_key(&key) {
                issues.push(LinkCheckIssue {
                    id: format!("symlink:unrecorded:{key}"),
                    key,
                    kind: LinkCheckKind::UnrecordedSymlink,
                    corresponding_container_uid: None,
                    corresponding_container_path: None,
                    linker_key: None,
                    target_key: None,
                    expected: None,
                    actual: Some(actual.display().to_string()),
                    actions: vec![
                        CheckRepairAction::DeleteUnrecordedSymlink,
                        CheckRepairAction::Skip,
                    ],
                });
            }
        }
        if issues.is_empty() {
            debug!("link consistency check completed without issues");
        } else {
            warn!(
                issue_count = issues.len(),
                "link consistency check found issues"
            );
        }
        Ok(issues)
    }

    fn repair_link_issue(
        &self,
        issue: &LinkCheckIssue,
        action: CheckRepairAction,
    ) -> Result<CheckActionResult> {
        let metadata = self.outgoing_metadata()?;
        let root = self
            .metadata_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("link container has no root directory"))?;
        let path = root.join(&issue.key);
        match (issue.kind.clone(), action) {
            (LinkCheckKind::UnrecordedSymlink, CheckRepairAction::DeleteUnrecordedSymlink) => {
                let file_type = fs::symlink_metadata(&path)?.file_type();
                if !file_type.is_symlink() {
                    bail!("{} is no longer a symlink", issue.key);
                }
                fs::remove_file(path)?;
            }
            (LinkCheckKind::MissingSymlink, CheckRepairAction::CreateMissingSymlink)
            | (LinkCheckKind::IncorrectSymlink, CheckRepairAction::ReplaceIncorrectSymlink) => {
                let link = metadata
                    .links
                    .get(&issue.key)
                    .ok_or_else(|| anyhow!("link metadata disappeared: {}", issue.key))?;
                if let Ok(file_type) = fs::symlink_metadata(&path) {
                    if !file_type.file_type().is_symlink() {
                        bail!("refusing to replace non-symlink entry {}", issue.key);
                    }
                    fs::remove_file(&path)?;
                }
                materialize_symlink(&self.root, &path, link, link.container_path.is_relative())?;
            }
            _ => {
                return Err(CheckActionError::InvalidAction {
                    issue_id: issue.id.clone(),
                    action,
                }
                .into());
            }
        }
        info!(entry_key = %issue.key, action = ?issue.kind, "applied link-check repair");
        Ok(CheckActionResult {
            description: match action {
                CheckRepairAction::CreateMissingSymlink => {
                    format!("created missing symlink {}", issue.key)
                }
                CheckRepairAction::ReplaceIncorrectSymlink => {
                    format!("replaced incorrect symlink {}", issue.key)
                }
                CheckRepairAction::DeleteUnrecordedSymlink => {
                    format!("deleted unrecorded symlink {}", issue.key)
                }
                _ => unreachable!("invalid actions returned above"),
            },
        })
    }

    /// Links `linker_key` in this container to `target_key` in `target`.
    /// The linker write lock remains held throughout the operation.
    pub fn link_to(
        &mut self,
        linker_key: &EntryKey,
        target: &mut dyn Container,
        target_key: &EntryKey,
        prefer_relative: Option<bool>,
    ) -> std::result::Result<(), LinkToError> {
        let span = container_operation_span!(self.logger, "link_to");
        let _entered = span.enter();
        debug!(linker_key = %linker_key, target_key = %target_key, "creating outgoing link");
        let target_uid = target.uid()?;
        let target_root = absolute_path(&target.root_path())?;
        let metadata = self.outgoing_metadata()?;
        let prefer_relative = prefer_relative.unwrap_or(metadata.prefer_relative);
        let container_path =
            recorded_container_path(&absolute_path(&self.root)?, &target_root, prefer_relative);
        let mut target_guard = target.writer().map_err(|error| {
            LinkAccessError::Unavailable(LinkUnavailableError {
                key: linker_key.clone(),
                container_uid: target_uid.clone(),
                container_path: target_root.clone(),
                reason: error.to_string(),
            })
        })?;
        let target_filename = absolute_path(&target_guard.filepath(target_key)?)?;
        if !target_filename.is_file() {
            return Err(LinkToError::TargetNotFound(target_key.clone()));
        }
        target_guard.link_from(&self.uid, target_key, linker_key)?;

        let result = self.install_outgoing_link(linker_key, target_key, target_uid, container_path);
        if let Err(error) = result {
            warn!(linker_key = %linker_key, target_key = %target_key, %error, "outgoing link installation failed; rolling back incoming record");
            if let Err(rollback) = target_guard.unlink(&self.uid, target_key, linker_key) {
                warn!(linker_key = %linker_key, target_key = %target_key, %rollback, "failed to roll back incoming record");
                return Err(LinkPartialCommitError {
                    operation: "link",
                    cause: error.to_string(),
                    completed_steps: vec![format!(
                        "registered incoming {}:{} on target {}",
                        self.uid, linker_key, target_key
                    )],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error);
        }
        info!(linker_key = %linker_key, target_key = %target_key, "created outgoing link");
        Ok(())
    }

    /// Removes an outgoing link and its reciprocal incoming-link record while
    /// retaining the linker write lock.
    pub fn unlink_to(&mut self, linker_key: &EntryKey) -> std::result::Result<(), UnlinkToError> {
        let metadata = self.outgoing_metadata()?;
        let link = metadata
            .links
            .get(linker_key)
            .cloned()
            .ok_or(UnlinkToError::LinkNotFound)?;
        let link_path = self.entry_path(linker_key)?;
        let (mut target_guard, _) =
            validate_outgoing_link_locked(&self.root, linker_key, &self.uid, &link_path, &link)?;
        target_guard.unlink(&self.uid, &link.target_key, linker_key)?;
        let result = self.remove_outgoing_link(linker_key);
        if let Err(error) = result {
            warn!(linker_key = %linker_key, target_key = %link.target_key, %error, "local outgoing-link removal failed; restoring incoming record");
            if let Err(rollback) = target_guard.link_from(&self.uid, &link.target_key, linker_key) {
                warn!(linker_key = %linker_key, target_key = %link.target_key, %rollback, "failed to restore incoming record");
                return Err(LinkPartialCommitError {
                    operation: "unlink",
                    cause: error.to_string(),
                    completed_steps: vec![format!(
                        "removed incoming {}:{} from target {}",
                        self.uid, linker_key, link.target_key
                    )],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error);
        }
        info!(linker_key = %linker_key, target_key = %link.target_key, "removed outgoing link");
        Ok(())
    }

    pub fn link_copy(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), LinkToError> {
        let metadata = self.outgoing_metadata().map_err(LinkToError::Other)?;
        let link = metadata
            .links
            .get(from)
            .cloned()
            .ok_or_else(|| LinkToError::TargetNotFound(from.clone()))?;
        let link_path = self.entry_path(from).map_err(LinkToError::Other)?;
        let (mut target_guard, _) =
            validate_outgoing_link_locked(&self.root, from, &self.uid, &link_path, &link)?;
        target_guard.link_from(&self.uid, &link.target_key, to)?;
        let result = self.install_outgoing_link(
            to,
            &link.target_key,
            link.container_uid.clone(),
            link.container_path.clone(),
        );
        if let Err(error) = result {
            if let Err(rollback) = target_guard.unlink(&self.uid, &link.target_key, to) {
                return Err(LinkPartialCommitError {
                    operation: "link-copy",
                    cause: error.to_string(),
                    completed_steps: vec![format!(
                        "registered incoming {}:{} on target {}",
                        self.uid, to, link.target_key
                    )],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error);
        }
        info!(from = %from, to = %to, target_key = %link.target_key, "copied outgoing link");
        Ok(())
    }

    pub fn link_rename(&mut self, from: &EntryKey, to: &EntryKey) -> Result<()> {
        let metadata = self.outgoing_metadata()?;
        let link = metadata
            .links
            .get(from)
            .cloned()
            .ok_or_else(|| anyhow!("outgoing link does not exist: {from}"))?;
        if from == to {
            return Ok(());
        }
        let (mut target_guard, _) = validate_outgoing_link_locked(
            &self.root,
            from,
            &self.uid,
            &self.entry_path(from)?,
            &link,
        )
        .map_err(LinkAccessError::into_anyhow)?;
        if self.outgoing_metadata()?.links.contains_key(to)
            || fs::symlink_metadata(self.entry_path(to)?).is_ok()
        {
            bail!("destination entry already exists: {to}");
        }

        target_guard.rename_link_from(&self.uid, &link.target_key, from, to)?;
        if let Err(error) = self.install_outgoing_link(
            to,
            &link.target_key,
            link.container_uid.clone(),
            link.container_path.clone(),
        ) {
            if let Err(rollback) =
                target_guard.rename_link_from(&self.uid, &link.target_key, to, from)
            {
                return Err(LinkPartialCommitError {
                    operation: "link-rename",
                    cause: error.to_string(),
                    completed_steps: vec![format!(
                        "atomically renamed incoming key {from} to {to}"
                    )],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error.into());
        }

        if let Err(error) = self.remove_outgoing_link(from) {
            let mut rollback_errors = Vec::new();
            if let Err(rollback) = self.remove_outgoing_link(to) {
                rollback_errors.push(rollback.to_string());
            }
            if let Err(rollback) =
                target_guard.rename_link_from(&self.uid, &link.target_key, to, from)
            {
                rollback_errors.push(rollback.to_string());
            }
            if rollback_errors.is_empty() {
                return Err(anyhow!(
                    "failed to remove old outgoing key {from}: {error}; transaction rolled back"
                ));
            }
            return Err(LinkPartialCommitError {
                operation: "link-rename",
                cause: error.to_string(),
                completed_steps: vec![
                    format!("atomically renamed incoming key {from} to {to}"),
                    format!("installed new outgoing key {to}"),
                ],
                rollback_errors,
            }
            .into());
        }
        info!(from = %from, to = %to, target_key = %link.target_key, "renamed outgoing link");
        Ok(())
    }

    fn install_outgoing_link(
        &mut self,
        linker_key: &EntryKey,
        target_key: &EntryKey,
        container_uid: String,
        container_path: PathBuf,
    ) -> std::result::Result<(), LinkToError> {
        let link_path = self.entry_path(linker_key)?;
        let mut metadata = self.outgoing_metadata()?;
        if let Some(existing) = metadata.links.get(linker_key) {
            return if existing.container_uid == container_uid
                && existing.target_key == *target_key
                && existing.container_path == container_path
            {
                Err(LinkToError::AlreadyLinked)
            } else {
                Err(LinkToError::LinkConflict)
            };
        }
        if fs::symlink_metadata(&link_path).is_ok() {
            return Err(LinkToError::LocalEntryExists(linker_key.clone()));
        }
        let link = OutgoingLink {
            target_key: target_key.clone(),
            container_uid,
            container_path,
        };
        materialize_symlink(
            &self.root,
            &link_path,
            &link,
            link.container_path.is_relative(),
        )?;
        metadata.links.insert(linker_key.clone(), link);
        if let Err(error) = self.save_metadata(&metadata) {
            if let Err(rollback) = fs::remove_file(&link_path) {
                return Err(LinkPartialCommitError {
                    operation: "install-outgoing-link",
                    cause: error.to_string(),
                    completed_steps: vec![format!("created symlink {linker_key}")],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn remove_outgoing_link(
        &mut self,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkToError> {
        let link_path = self.entry_path(linker_key)?;
        let mut metadata = self.outgoing_metadata()?;
        let Some(link) = metadata.links.get(linker_key).cloned() else {
            return Err(UnlinkToError::LinkNotFound);
        };
        let removed_symlink = match fs::symlink_metadata(&link_path) {
            Ok(metadata) if !metadata.file_type().is_symlink() => {
                return Err(UnlinkToError::Other(anyhow!(
                    "refusing to remove non-symlink entry {}",
                    linker_key
                )));
            }
            Ok(_) => {
                fs::remove_file(&link_path).map_err(|error| UnlinkToError::Other(error.into()))?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(UnlinkToError::Other(error.into())),
        };
        metadata.links.remove(linker_key);
        if let Err(error) = self.save_metadata(&metadata) {
            if removed_symlink
                && let Err(rollback) = materialize_symlink(
                    &self.root,
                    &link_path,
                    &link,
                    link.container_path.is_relative(),
                )
            {
                return Err(LinkPartialCommitError {
                    operation: "remove-outgoing-link",
                    cause: error.to_string(),
                    completed_steps: vec![format!("removed symlink {linker_key}")],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(UnlinkToError::Other(error));
        }
        Ok(())
    }
}
