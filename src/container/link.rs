use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTROL_DIR, CheckActionResult, CheckRepairAction, Container,
    ContainerMetadata, ContainerWriteGuard, CopyError, EntryKey, LinkCheckIssue, LinkCheckKind,
    LinkFromError, LinkInfo, LinkMetadataMigrationError, LocalContainer,
    OUTGOING_LINKS_FORMAT_VERSION, RemoveError, RenameError, UnlinkError, WriteError, WriterError,
    open_container,
};

const METADATA_FILE: &str = "outgoing-links.json";

/// A local directory which can contain both ordinary files and outgoing links.
///
/// Ordinary storage and locking are provided by [`LocalContainer`]. Outgoing
/// link metadata in `.kcl/outgoing-links.json` is authoritative; filesystem
/// symlinks are only its materialized representation.
pub struct LinkContainer {
    local: LocalContainer,
    logger: ContainerLogger,
}

impl LinkContainer {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let logical_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("link")
            .to_owned();
        let local = LocalContainer::open_as(path, logical_name, "link")?;
        Self::from_local(local)
    }

    pub fn with_logical_name(
        path: impl Into<PathBuf>,
        logical_name: impl Into<String>,
    ) -> Result<Self> {
        let local = LocalContainer::open_as(path.into(), logical_name.into(), "link")?;
        Self::from_local(local)
    }

    /// Opens an existing link container from already parsed common metadata.
    pub fn from_metadata(path: impl Into<PathBuf>, metadata: ContainerMetadata) -> Result<Self> {
        let local = LocalContainer::from_metadata_as(path.into(), metadata, "link")?;
        Self::from_local(local)
    }

    pub fn path(&self) -> &Path {
        let span = container_operation_span!(self.logger, "path");
        let _entered = span.enter();
        let path = self.local.path_ref();
        trace!(path = %path.display(), "getting container path");
        path
    }

    /// Lists only ordinary local entries, excluding outgoing links.
    pub fn local_list(&self) -> Result<Vec<EntryKey>> {
        self.local.list()
    }

    pub fn local_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        self.local.list_info()
    }

    /// Reads an ordinary local entry without following an outgoing link.
    pub fn local_read(&self, key: &EntryKey) -> Result<Buffer> {
        self.guarded()?.local_read_verified(key)
    }

    /// Resolves the path of an ordinary local entry.
    pub fn local_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.guarded()?.local_filepath_verified(key)
    }

    /// Lists outgoing-link keys only.
    pub fn link_list(&self) -> Result<Vec<EntryKey>> {
        Ok(self.outgoing_metadata()?.links.into_keys().collect())
    }

    pub fn link_list_info(&self) -> Result<Vec<super::ContainerEntryInfo>> {
        Ok(self
            .list_info()?
            .into_iter()
            .filter(|entry| matches!(entry.link, LinkInfo::LinkTo { .. }))
            .collect())
    }

    pub fn prefer_relative(&self) -> Result<bool> {
        Ok(self.outgoing_metadata()?.prefer_relative)
    }

    fn from_local(local: LocalContainer) -> Result<Self> {
        let logger = ContainerLogger::new("link", local.uid()?);
        debug!(
            logger = %logger.name(),
            container_uid = %logger.uid(),
            container_kind = logger.kind(),
            operation = "open",
            path = %local.path_ref().display(),
            "opened container"
        );
        Ok(Self { local, logger })
    }

    fn outgoing_metadata_path(&self) -> PathBuf {
        self.local.path_ref().join(CONTROL_DIR).join(METADATA_FILE)
    }

    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        self.guarded()?.outgoing_metadata()
    }

    fn guarded(&self) -> std::result::Result<LinkContainerWriteGuard, WriterError> {
        Ok(LinkContainerWriteGuard {
            root: self.local.path_ref().to_owned(),
            uid: self.logger.uid().to_owned(),
            metadata_path: self.outgoing_metadata_path(),
            local: self.local.writer()?,
            logger: self.logger.clone(),
        })
    }

    /// Acquires the link container's exclusive writer. All outgoing-link
    /// mutations are methods on the returned guard.
    pub fn writer(&self) -> std::result::Result<LinkContainerWriteGuard, WriterError> {
        self.guarded()
    }
}

impl Container for LinkContainer {
    fn root_path(&self) -> PathBuf {
        self.local.path_ref().to_owned()
    }

    fn metadata(&self) -> ContainerMetadata {
        let span = container_operation_span!(self.logger, "metadata");
        let _entered = span.enter();
        trace!("getting container metadata");
        self.local.metadata()
    }

    fn uid(&self) -> Result<String> {
        let span = container_operation_span!(self.logger, "uid");
        let _entered = span.enter();
        trace!("getting container UID");
        Ok(self.logger.uid().to_owned())
    }

    fn logical_name(&self) -> String {
        let span = container_operation_span!(self.logger, "logical_name");
        let _entered = span.enter();
        trace!("getting container logical name");
        self.local.logical_name()
    }

    fn kind(&self) -> String {
        let span = container_operation_span!(self.logger, "kind");
        let _entered = span.enter();
        trace!("getting container kind");
        "link".to_owned()
    }

    fn list(&self) -> Result<Vec<EntryKey>> {
        let span = container_operation_span!(self.logger, "list");
        let _entered = span.enter();
        debug!("listing container entries");
        let mut keys: BTreeSet<_> = self.local.list()?.into_iter().collect();
        keys.extend(self.link_list()?);
        debug!(entry_count = keys.len(), "listed container entries");
        Ok(keys.into_iter().collect())
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        let span = container_operation_span!(self.logger, "filepath");
        let _entered = span.enter();
        debug!(entry_key = %key, "resolving entry path");
        self.guarded()?.filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let span = container_operation_span!(self.logger, "read");
        let _entered = span.enter();
        debug!(entry_key = %key, "reading container entry");
        self.guarded()?.read(key)
    }

    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError> {
        let span = container_operation_span!(self.logger, "writer");
        let _entered = span.enter();
        debug!("creating container writer");
        Ok(Box::new(self.guarded()?))
    }
}

pub struct LinkContainerWriteGuard {
    root: PathBuf,
    uid: String,
    metadata_path: PathBuf,
    local: Box<dyn ContainerWriteGuard>,
    logger: ContainerLogger,
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
        super::local::validate_key(key).map_err(|key| anyhow!("invalid entry key: {key}"))?;
        Ok(self.root.join(key))
    }

    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        read_metadata(&self.metadata_path)
    }

    fn save_metadata(&self, metadata: &OutgoingLinksMetadata) -> Result<()> {
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

    fn local_filepath_verified(&self, key: &EntryKey) -> Result<PathBuf> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.filepath(key)
    }

    fn local_read_verified(&self, key: &EntryKey) -> Result<Buffer> {
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
            _ => bail!(
                "action {action:?} is not valid for check issue {}",
                issue.id
            ),
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

        target_guard.link_from(&self.uid, &link.target_key, to)?;
        if let Err(error) = self.install_outgoing_link(
            to,
            &link.target_key,
            link.container_uid.clone(),
            link.container_path.clone(),
        ) {
            if let Err(rollback) = target_guard.unlink(&self.uid, &link.target_key, to) {
                return Err(LinkPartialCommitError {
                    operation: "link-rename",
                    cause: error.to_string(),
                    completed_steps: vec![format!("registered new incoming key {to}")],
                    rollback_errors: vec![rollback.to_string()],
                }
                .into());
            }
            return Err(error.into());
        }

        if let Err(error) = target_guard.unlink(&self.uid, &link.target_key, from) {
            let mut rollback_errors = Vec::new();
            if let Err(rollback) = self.remove_outgoing_link(to) {
                rollback_errors.push(rollback.to_string());
            }
            if let Err(rollback) = target_guard.unlink(&self.uid, &link.target_key, to) {
                rollback_errors.push(rollback.to_string());
            }
            if rollback_errors.is_empty() {
                return Err(error.into());
            }
            return Err(LinkPartialCommitError {
                operation: "link-rename",
                cause: error.to_string(),
                completed_steps: vec![
                    format!("registered new incoming key {to}"),
                    format!("installed new outgoing key {to}"),
                ],
                rollback_errors,
            }
            .into());
        }

        if let Err(error) = self.remove_outgoing_link(from) {
            let mut rollback_errors = Vec::new();
            if let Err(rollback) = target_guard.link_from(&self.uid, &link.target_key, from) {
                rollback_errors.push(rollback.to_string());
            }
            if let Err(rollback) = self.remove_outgoing_link(to) {
                rollback_errors.push(rollback.to_string());
            }
            if let Err(rollback) = target_guard.unlink(&self.uid, &link.target_key, to) {
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
                    format!("registered and installed new key {to}"),
                    format!("removed old incoming key {from}"),
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

impl ContainerWriteGuard for LinkContainerWriteGuard {
    fn container_uid(&self) -> &str {
        &self.uid
    }

    fn link_snapshot(&self) -> Result<super::ContainerLinkSnapshot> {
        let mut snapshot = self.local.link_snapshot()?;
        let metadata = self.outgoing_metadata()?;
        snapshot.container_uid = self.uid.clone();
        snapshot.container_path = self.root.clone();
        snapshot.outgoing = metadata
            .links
            .into_iter()
            .map(|(linker_key, link)| super::OutgoingLinkRecord {
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
        let validation = super::validation_check_issues(self, corresponding)?;
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
            _ => super::apply_validation_check_action(self, issue, action, corresponding),
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

#[derive(Debug, Error)]
pub enum LinkToError {
    #[error("the outgoing link is already registered")]
    AlreadyLinked,
    #[error("the key has a different outgoing link")]
    LinkConflict,
    #[error("a local entry already exists at: {0}")]
    LocalEntryExists(EntryKey),
    #[error("target entry does not exist: {0}")]
    TargetNotFound(EntryKey),
    #[error(transparent)]
    Target(#[from] LinkFromError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum UnlinkToError {
    #[error("the outgoing link is not registered")]
    LinkNotFound,
    #[error("container does not match the recorded link")]
    ContainerMismatch,
    #[error(transparent)]
    Target(#[from] UnlinkError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error(transparent)]
    Access(#[from] LinkAccessError),
    #[error(transparent)]
    PartialCommit(#[from] LinkPartialCommitError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
#[error(
    "partial commit during {operation}: {cause}; completed steps: {completed_steps:?}; rollback errors: {rollback_errors:?}"
)]
pub struct LinkPartialCommitError {
    pub operation: &'static str,
    pub cause: String,
    pub completed_steps: Vec<String>,
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Error)]
pub enum LinkAccessError {
    #[error(transparent)]
    Broken(#[from] BrokenLinkError),
    #[error(transparent)]
    Unavailable(#[from] LinkUnavailableError),
}

impl LinkAccessError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Broken(error) => error.into(),
            Self::Unavailable(error) => error.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("broken outgoing link '{key}': {reason}")]
pub struct BrokenLinkError {
    pub key: EntryKey,
    pub reason: String,
}

impl BrokenLinkError {
    fn new(key: &EntryKey, reason: impl Into<String>) -> Self {
        Self {
            key: key.clone(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error(
    "outgoing link '{key}' is temporarily unavailable: target {container_uid} at {container_path}: {reason}"
)]
pub struct LinkUnavailableError {
    pub key: EntryKey,
    pub container_uid: String,
    pub container_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OutgoingLinksMetadata {
    version: u32,
    #[serde(default = "default_prefer_relative")]
    prefer_relative: bool,
    links: BTreeMap<EntryKey, OutgoingLink>,
}

impl Default for OutgoingLinksMetadata {
    fn default() -> Self {
        Self {
            version: OUTGOING_LINKS_FORMAT_VERSION,
            prefer_relative: true,
            links: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutgoingLink {
    target_key: EntryKey,
    container_uid: String,
    container_path: PathBuf,
}

fn default_prefer_relative() -> bool {
    true
}

fn read_metadata(path: &Path) -> Result<OutgoingLinksMetadata> {
    if !path.exists() {
        return Ok(OutgoingLinksMetadata::default());
    }
    let value: serde_json::Value = serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| migration_failed(path, "metadata has no numeric version"))?
        as u32;
    match version {
        OUTGOING_LINKS_FORMAT_VERSION => serde_json::from_value(value)
            .with_context(|| format!("failed to parse {}", path.display())),
        1 => migrate_outgoing_v1(path, value),
        version => Err(LinkMetadataMigrationError::Required {
            path: path.to_owned(),
            version,
        }
        .into()),
    }
}

fn migrate_outgoing_v1(path: &Path, value: serde_json::Value) -> Result<OutgoingLinksMetadata> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| migration_failed(path, "metadata path has no container root"))?;
    let prefer_relative = value
        .get("prefer_relative")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let links = value
        .get("links")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| migration_failed(path, "metadata has no links object"))?;
    let mut migrated = BTreeMap::new();
    for (linker_key, raw) in links {
        let target_key = raw
            .get("target_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| migration_failed(path, format!("{linker_key} has no target_key")))?
            .to_owned();
        let container_uid = raw
            .get("container_uid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| migration_failed(path, format!("{linker_key} has no container_uid")))?
            .to_owned();
        let container_path = if let Some(recorded) = raw.get("container_path") {
            serde_json::from_value::<PathBuf>(recorded.clone()).map_err(|error| {
                migration_failed(
                    path,
                    format!("invalid container_path for {linker_key}: {error}"),
                )
            })?
        } else {
            let target_filename = raw
                .get("target_filename")
                .cloned()
                .ok_or_else(|| {
                    migration_failed(
                        path,
                        format!("{linker_key} has neither container_path nor target_filename"),
                    )
                })
                .and_then(|value| {
                    serde_json::from_value::<PathBuf>(value).map_err(|error| {
                        migration_failed(
                            path,
                            format!("invalid target_filename for {linker_key}: {error}"),
                        )
                    })
                })?;
            let target_key_path = Path::new(&target_key);
            if !target_filename.ends_with(target_key_path) {
                return Err(migration_failed(
                    path,
                    format!(
                        "target_filename {} does not end with target key {}",
                        target_filename.display(),
                        target_key
                    ),
                ));
            }
            let mut target_root = target_filename.clone();
            for _ in target_key_path.components() {
                if !target_root.pop() {
                    return Err(migration_failed(
                        path,
                        format!("cannot derive target root for {linker_key}"),
                    ));
                }
            }
            let old_prefer_relative = raw
                .get("prefer_relative")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(prefer_relative);
            recorded_container_path(
                &absolute_path(root)?,
                &absolute_path(&target_root)?,
                old_prefer_relative,
            )
        };
        let resolved = if container_path.is_absolute() {
            container_path.clone()
        } else {
            root.join(&container_path)
        };
        let actual_uid = open_container(&resolved)
            .and_then(|container| container.uid())
            .map_err(|error| {
                migration_failed(
                    path,
                    format!(
                        "cannot verify target container for {linker_key} at {}: {error:#}",
                        resolved.display()
                    ),
                )
            })?;
        if actual_uid != container_uid {
            return Err(migration_failed(
                path,
                format!(
                    "target UID mismatch for {linker_key}: recorded {container_uid}, found {actual_uid}"
                ),
            ));
        }
        migrated.insert(
            linker_key.clone(),
            OutgoingLink {
                target_key,
                container_uid,
                container_path,
            },
        );
    }
    let metadata = OutgoingLinksMetadata {
        version: OUTGOING_LINKS_FORMAT_VERSION,
        prefer_relative,
        links: migrated,
    };
    backup_v1(path)?;
    write_json_atomic(path, &metadata).map_err(|error| {
        migration_failed(path, format!("failed to write v2 metadata: {error:#}"))
    })?;
    info!(path = %path.display(), link_count = metadata.links.len(), "migrated outgoing link metadata to v2");
    Ok(metadata)
}

fn migration_failed(path: &Path, reason: impl Into<String>) -> anyhow::Error {
    LinkMetadataMigrationError::Failed {
        path: path.to_owned(),
        reason: reason.into(),
    }
    .into()
}

fn backup_v1(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| migration_failed(path, "metadata path has no file name"))?;
    let backup = path.with_file_name(format!("{}.v1.backup", file_name.to_string_lossy()));
    if backup.exists() && !backup.is_file() {
        return Err(migration_failed(
            path,
            format!("backup path is not a file: {}", backup.display()),
        ));
    }
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|error| {
            migration_failed(
                path,
                format!("failed to create backup {}: {error}", backup.display()),
            )
        })?;
        File::open(&backup)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                migration_failed(
                    path,
                    format!("failed to sync backup {}: {error}", backup.display()),
                )
            })?;
    }
    Ok(backup)
}

fn validate_outgoing_link_locked(
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
    Ok((target_guard, target_path))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result: Result<()> = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist {}", path.display()))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn recorded_container_path(root: &Path, target_root: &Path, prefer_relative: bool) -> PathBuf {
    if prefer_relative && let Some(relative) = pathdiff::diff_paths(target_root, root) {
        return relative;
    }
    target_root.to_owned()
}

fn resolved_container_path(root: &Path, link: &OutgoingLink) -> PathBuf {
    if link.container_path.is_absolute() {
        link.container_path.clone()
    } else {
        root.join(&link.container_path)
    }
}

fn target_filename(root: &Path, link: &OutgoingLink) -> PathBuf {
    resolved_container_path(root, link).join(&link.target_key)
}

fn symlink_target(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> PathBuf {
    let target_filename = target_filename(root, link);
    if prefer_relative
        && let Some(parent) = link_path.parent()
        && let Some(relative) = pathdiff::diff_paths(&target_filename, parent)
    {
        return relative;
    }
    target_filename
}

fn materialize_symlink(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> Result<()> {
    let parent = link_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", link_path.display()))?;
    fs::create_dir_all(parent)?;
    let target = symlink_target(root, link_path, link, prefer_relative);
    create_file_symlink(&target, link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

fn verify_materialized_symlink(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> Result<()> {
    let actual = fs::read_link(link_path).with_context(|| {
        format!(
            "outgoing link metadata exists but {} is not a symlink",
            link_path.display()
        )
    })?;
    let expected = symlink_target(root, link_path, link, prefer_relative);
    if actual == expected {
        Ok(())
    } else {
        Err(anyhow!(
            "symlink {} points to {}, metadata requires {}",
            link_path.display(),
            actual.display(),
            expected.display()
        ))
    }
}

fn collect_symlinks(
    root: &Path,
    directory: &Path,
    symlinks: &mut BTreeMap<EntryKey, PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
    {
        let entry = entry?;
        if directory == root && entry.file_name() == CONTROL_DIR {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_symlinks(root, &path, symlinks)?;
        } else if file_type.is_symlink() {
            let key = path
                .strip_prefix(root)
                .expect("symlink is below container root")
                .to_string_lossy()
                .into_owned();
            symlinks.insert(key, fs::read_link(&path)?);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
