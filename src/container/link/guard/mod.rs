//! Link-container write guard and multi-container mutation transactions.

use super::*;

mod container;

/// Exclusive write guard for one link container.
///
/// The guard owns the current container lock and is the only public surface
/// for changing its local entries, outgoing metadata, or symbolic links.
pub struct LinkContainerWriteGuard {
    /// Root directory containing ordinary entries and materialized outgoing symlinks.
    pub(super) root: PathBuf,
    /// Persistent UID of the locked link container.
    pub(super) uid: String,
    /// Full path of the authoritative `.kcl/outgoing-links.json` document.
    pub(super) metadata_path: PathBuf,
    /// Underlying local guard that owns the advisory lock and ordinary-entry operations.
    pub(super) local: Box<dyn ContainerWriteGuard>,
    /// Stable link-container identity attached to guarded-operation tracing spans.
    pub(super) logger: ContainerLogger,
}

impl LinkContainerWriteGuard {
    /// Sets the default path policy used by subsequent [`Self::link_to`] calls.
    ///
    /// # Arguments
    ///
    /// * `prefer_relative` - `true` to record relative target-container paths
    ///   whenever calculable; `false` to record absolute paths.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the container-level preference is persisted.
    ///
    /// # Errors
    ///
    /// Returns an error if changing the preference would reinterpret existing
    /// outgoing links or metadata cannot be read or persisted.
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
    /// Validates a key and resolves its path below the locked container root.
    ///
    /// # Arguments
    ///
    /// * `key` - Root-relative ordinary or outgoing-link entry key.
    ///
    /// # Returns
    ///
    /// The container root joined with `key`.
    ///
    /// # Errors
    ///
    /// Returns an error when `key` is empty, absolute, escapes the root, or enters `.kcl`.
    fn entry_path(&self, key: &EntryKey) -> Result<PathBuf> {
        super::super::local::validate_key(key)
            .map_err(|key| anyhow!("invalid entry key: {key}"))?;
        Ok(self.root.join(key))
    }

    /// Loads authoritative outgoing metadata for the locked container.
    ///
    /// # Returns
    ///
    /// Parsed current metadata, including any safely migrated version-one records.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read, parsed, validated, or migrated.
    pub(super) fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        read_metadata(&self.metadata_path)
    }

    /// Atomically persists authoritative outgoing metadata.
    ///
    /// # Arguments
    ///
    /// * `metadata` - Complete current-version document that will replace the existing file.
    ///
    /// # Returns
    ///
    /// `Ok(())` after synchronized JSON is installed at `metadata_path`.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or filesystem persistence fails.
    fn save_metadata(&self, metadata: &OutgoingLinksMetadata) -> Result<()> {
        #[cfg(test)]
        if should_fail_outgoing_metadata_write() {
            bail!("injected outgoing metadata write failure");
        }
        write_json_atomic(&self.metadata_path, metadata)
    }

    /// Resolves an entry path after validating an outgoing relationship when present.
    ///
    /// # Arguments
    ///
    /// * `key` - Ordinary or outgoing-link entry key to resolve.
    ///
    /// # Returns
    ///
    /// The materialized path below this container root. For an outgoing key, the target container
    /// remains locked only during validation and is released before return.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid key, unreadable metadata, broken link state, or unavailable
    /// target writer.
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

    /// Resolves only an ordinary local entry path without following outgoing links.
    ///
    /// # Arguments
    ///
    /// * `key` - Candidate ordinary entry key.
    ///
    /// # Returns
    ///
    /// The ordinary entry path supplied by the underlying local guard.
    ///
    /// # Errors
    ///
    /// Returns an error if outgoing metadata cannot be read, `key` is an outgoing link, or local
    /// key validation fails.
    pub(super) fn local_filepath_verified(&self, key: &EntryKey) -> Result<PathBuf> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.filepath(key)
    }

    /// Reads only ordinary local content without following outgoing links.
    ///
    /// # Arguments
    ///
    /// * `key` - Candidate ordinary entry key.
    ///
    /// # Returns
    ///
    /// A newly allocated buffer containing the local entry bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if outgoing metadata cannot be read, `key` is an outgoing link, or the
    /// ordinary entry cannot be validated or read.
    pub(super) fn local_read_verified(&self, key: &EntryKey) -> Result<Buffer> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.read(key)
    }

    /// Reads an ordinary entry or a fully validated outgoing target.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key to read from local storage or through an outgoing relationship.
    ///
    /// # Returns
    ///
    /// A newly allocated buffer containing the selected local or target entry bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable metadata or local data, invalid keys, broken relationship
    /// identity or symlink state, target lock unavailability, or target read failure.
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

    /// Compares authoritative outgoing records with all materialized symlinks.
    ///
    /// # Returns
    ///
    /// Deterministic issues for missing, incorrect, and unrecorded symlinks, each with concrete
    /// actions supported by [`Self::repair_link_issue`].
    ///
    /// # Errors
    ///
    /// Returns an error if metadata, directories, file types, or symbolic-link targets cannot be
    /// read.
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

    /// Applies one explicitly selected filesystem repair for a link check issue.
    ///
    /// # Arguments
    ///
    /// * `issue` - Previously reported missing, incorrect, or unrecorded symlink issue.
    /// * `action` - Concrete action offered by the issue, including `Skip`.
    ///
    /// # Returns
    ///
    /// A human-readable description of the performed mutation or skipped issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the action does not match the issue, metadata changed, a non-symlink
    /// would be overwritten, or symbolic-link removal or creation fails.
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
    ///
    /// # Arguments
    ///
    /// * `linker_key` - New outgoing key to occupy in the locked link
    ///   container.
    /// * `target` - Target container whose writer will be acquired
    ///   non-blockingly and whose reciprocal incoming record will be updated.
    /// * `target_key` - Existing ordinary entry key in `target`.
    /// * `prefer_relative` - Per-link path-policy override: `Some(true)`
    ///   prefers relative, `Some(false)` forces absolute, and `None` reuses the
    ///   container default.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the reciprocal incoming record, outgoing metadata, and
    /// materialized symbolic link are all installed while locks are held.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for key conflicts, missing targets, unavailable
    /// target locks, broken existing state, persistence failures, or an
    /// incomplete rollback reported as partial commit.
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
        let mut target_guard = target.writer().map_err(|error| {
            LinkAccessError::Unavailable(LinkUnavailableError {
                key: linker_key.clone(),
                container_uid: target_uid.clone(),
                container_path: target_root.clone(),
                reason: error.to_string(),
            })
        })?;
        self.link_to_locked(
            linker_key,
            &target_root,
            target_guard.as_mut(),
            target_key,
            prefer_relative,
        )
    }

    /// Creates an outgoing relationship using an already acquired target writer.
    ///
    /// This entry point lets a higher-level transaction acquire source and target
    /// locks in a stable order without asking this guard to lock the target again.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - New outgoing key in this locked link-capable Container.
    /// * `target_root` - Filesystem root represented by `target_guard`.
    /// * `target_guard` - Already locked target Container writer.
    /// * `target_key` - Existing ordinary target entry.
    /// * `prefer_relative` - Per-link path override, or `None` for this Container's default.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal metadata, outgoing metadata, and symlink are installed.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for missing/conflicting targets, metadata or
    /// filesystem failures, or incomplete rollback.
    pub fn link_to_locked(
        &mut self,
        linker_key: &EntryKey,
        target_root: &Path,
        target_guard: &mut dyn ContainerWriteGuard,
        target_key: &EntryKey,
        prefer_relative: Option<bool>,
    ) -> std::result::Result<(), LinkToError> {
        let target_uid = target_guard.container_uid().to_owned();
        let target_root = absolute_path(target_root)?;
        let metadata = self.outgoing_metadata()?;
        let prefer_relative = prefer_relative.unwrap_or(metadata.prefer_relative);
        let container_path =
            recorded_container_path(&absolute_path(&self.root)?, &target_root, prefer_relative);
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
    ///
    /// # Arguments
    ///
    /// * `linker_key` - Existing outgoing key to validate and remove.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal incoming metadata, local outgoing metadata,
    /// and the symbolic link are removed while both required locks are held.
    ///
    /// # Errors
    ///
    /// Returns [`UnlinkToError`] when the link is absent, broken, unavailable,
    /// cannot be persisted, or cannot be fully rolled back after failure.
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
        self.unlink_to_locked(linker_key, target_guard.as_mut())
    }

    /// Removes an outgoing relationship using an already acquired target writer.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - Existing outgoing key in this locked Container.
    /// * `target_guard` - Already locked target writer selected from the outgoing record.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal and local outgoing state are removed.
    ///
    /// # Errors
    ///
    /// Returns [`UnlinkToError`] when the record is absent or broken, target
    /// state mismatches, persistence fails, or rollback is incomplete.
    pub fn unlink_to_locked(
        &mut self,
        linker_key: &EntryKey,
        target_guard: &mut dyn ContainerWriteGuard,
    ) -> std::result::Result<(), UnlinkToError> {
        let metadata = self.outgoing_metadata()?;
        let link = metadata
            .links
            .get(linker_key)
            .cloned()
            .ok_or(UnlinkToError::LinkNotFound)?;
        super::access::validate_outgoing_link_with_guard(
            &self.root,
            linker_key,
            &self.uid,
            &self.entry_path(linker_key)?,
            &link,
            target_guard,
        )?;
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

    /// Validates one outgoing relationship against an already locked target writer.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - Existing outgoing key in this locked link-capable Container.
    /// * `target_guard` - Already acquired target writer selected by the outgoing record.
    ///
    /// # Returns
    ///
    /// `Ok(())` only when target UID/key, reciprocal metadata, and symlink all match.
    ///
    /// # Errors
    ///
    /// Returns [`LinkAccessError::Broken`] when any persistent relationship state mismatches,
    /// or [`LinkAccessError::Unavailable`] only if validation delegated by a target implementation
    /// reports temporary unavailability.
    pub fn validate_outgoing_locked(
        &self,
        linker_key: &EntryKey,
        target_guard: &dyn ContainerWriteGuard,
    ) -> std::result::Result<(), LinkAccessError> {
        let metadata = self
            .outgoing_metadata()
            .map_err(|error| BrokenLinkError::new(linker_key, error.to_string()))?;
        let link = metadata
            .links
            .get(linker_key)
            .ok_or_else(|| BrokenLinkError::new(linker_key, "outgoing link metadata is missing"))?;
        super::access::validate_outgoing_link_with_guard(
            &self.root,
            linker_key,
            &self.uid,
            &self
                .entry_path(linker_key)
                .map_err(|error| BrokenLinkError::new(linker_key, error.to_string()))?,
            link,
            target_guard,
        )?;
        Ok(())
    }

    /// Copies one validated outgoing relationship to a new linker key.
    ///
    /// The copy preserves the source record's target UID, target key, and
    /// relative-versus-absolute path representation.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key whose relationship must validate.
    /// * `to` - Unoccupied outgoing key to create.
    ///
    /// # Returns
    ///
    /// `Ok(())` after a second reciprocal incoming source, outgoing record,
    /// and symbolic link are installed while locks are held.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for missing/broken source links, occupied
    /// destination, unavailable targets, persistence failure, or partial
    /// commit.
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
        self.link_copy_locked(from, to, target_guard.as_mut())
    }

    /// Copies an outgoing relationship using an already acquired target writer.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key to validate and copy.
    /// * `to` - Unoccupied outgoing destination key.
    /// * `target_guard` - Already locked target writer selected by the source record.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal and outgoing state contains the additional key.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for broken source state, conflicts, persistence
    /// failures, or incomplete rollback.
    pub fn link_copy_locked(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
        target_guard: &mut dyn ContainerWriteGuard,
    ) -> std::result::Result<(), LinkToError> {
        let metadata = self.outgoing_metadata().map_err(LinkToError::Other)?;
        let link = metadata
            .links
            .get(from)
            .cloned()
            .ok_or_else(|| LinkToError::TargetNotFound(from.clone()))?;
        super::access::validate_outgoing_link_with_guard(
            &self.root,
            from,
            &self.uid,
            &self.entry_path(from).map_err(LinkToError::Other)?,
            &link,
            target_guard,
        )?;
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

    /// Renames a validated outgoing link without changing its target or path policy.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key to rename.
    /// * `to` - Unoccupied destination outgoing key.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal metadata and both local outgoing key states
    /// reflect the new key while locks remain held.
    ///
    /// # Errors
    ///
    /// Returns an error if the source is absent/broken, the destination is
    /// occupied, a target lock is unavailable, persistence fails, or rollback
    /// cannot fully restore a consistent state.
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
        self.link_rename_locked(from, to, target_guard.as_mut())
    }

    /// Renames an outgoing relationship using an already acquired target writer.
    ///
    /// # Arguments
    ///
    /// * `from` - Existing outgoing key.
    /// * `to` - Unoccupied replacement key.
    /// * `target_guard` - Already locked target writer selected by the source record.
    ///
    /// # Returns
    ///
    /// `Ok(())` after reciprocal and local outgoing state use `to`.
    ///
    /// # Errors
    ///
    /// Returns an error for broken state, conflicts, persistence failures, or
    /// incomplete rollback.
    pub fn link_rename_locked(
        &mut self,
        from: &EntryKey,
        to: &EntryKey,
        target_guard: &mut dyn ContainerWriteGuard,
    ) -> Result<()> {
        let metadata = self.outgoing_metadata()?;
        let link = metadata
            .links
            .get(from)
            .cloned()
            .ok_or_else(|| anyhow!("outgoing link does not exist: {from}"))?;
        if from == to {
            return Ok(());
        }
        super::access::validate_outgoing_link_with_guard(
            &self.root,
            from,
            &self.uid,
            &self.entry_path(from)?,
            &link,
            target_guard,
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

    /// Installs one outgoing symlink and its authoritative metadata record.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - New entry key in this link container.
    /// * `target_key` - Ordinary entry key in the target container.
    /// * `container_uid` - Verified persistent UID of the target container.
    /// * `container_path` - Target root representation to persist, relative or absolute.
    ///
    /// # Returns
    ///
    /// `Ok(())` after both the symlink and outgoing metadata are installed.
    ///
    /// # Errors
    ///
    /// Returns [`LinkToError`] for invalid or occupied keys, conflicting records, filesystem or
    /// metadata failure, or an incomplete rollback after metadata persistence fails.
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

    /// Removes one outgoing symlink and its authoritative metadata record.
    ///
    /// # Arguments
    ///
    /// * `linker_key` - Existing outgoing entry key to remove locally.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the symlink, when present, and metadata record are removed.
    ///
    /// # Errors
    ///
    /// Returns [`UnlinkToError`] when the record is absent, the path holds a non-symlink, filesystem
    /// or metadata work fails, or a removed symlink cannot be restored after persistence failure.
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
