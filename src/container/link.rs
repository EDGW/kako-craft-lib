use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    AddError, Buffer, CONTAINER_FORMAT_VERSION, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, CopyError, EntryKey, LinkCheckIssue, LinkCheckKind, LinkFromError,
    LinkInfo, LocalContainer, RemoveError, RenameError, UnlinkError, WriteError, WriterError,
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
        debug!(path = %path.display(), "getting container path");
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
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.read(key)
    }

    /// Resolves the path of an ordinary local entry.
    pub fn local_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        if self.outgoing_metadata()?.links.contains_key(key) {
            return Err(anyhow!("entry is an outgoing link: {key}"));
        }
        self.local.filepath(key)
    }

    /// Lists outgoing-link keys only.
    pub fn link_list(&self) -> Result<Vec<EntryKey>> {
        let metadata = self.outgoing_metadata()?;
        for (key, link) in &metadata.links {
            validate_outgoing_link(
                self.local.path_ref(),
                key,
                self.logger.uid(),
                &self.local.filepath(key)?,
                link,
                link.container_path.is_relative(),
            )?;
        }
        Ok(metadata.links.into_keys().collect())
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
        read_metadata(&self.outgoing_metadata_path())
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
        debug!("getting container metadata");
        self.local.metadata()
    }

    fn uid(&self) -> Result<String> {
        let span = container_operation_span!(self.logger, "uid");
        let _entered = span.enter();
        debug!("getting container UID");
        Ok(self.logger.uid().to_owned())
    }

    fn logical_name(&self) -> String {
        let span = container_operation_span!(self.logger, "logical_name");
        let _entered = span.enter();
        debug!("getting container logical name");
        self.local.logical_name()
    }

    fn kind(&self) -> String {
        let span = container_operation_span!(self.logger, "kind");
        let _entered = span.enter();
        debug!("getting container kind");
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
        let path = self.local.filepath(key)?;
        let metadata = self.outgoing_metadata()?;
        if let Some(link) = metadata.links.get(key) {
            validate_outgoing_link(
                self.local.path_ref(),
                key,
                self.logger.uid(),
                &path,
                link,
                link.container_path.is_relative(),
            )?;
        }
        Ok(path)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let span = container_operation_span!(self.logger, "read");
        let _entered = span.enter();
        debug!(entry_key = %key, "reading container entry");
        let metadata = self.outgoing_metadata()?;
        if let Some(link) = metadata.links.get(key) {
            let link_path = self.local.filepath(key)?;
            let (target, _) = validate_outgoing_link(
                self.local.path_ref(),
                key,
                self.logger.uid(),
                &link_path,
                link,
                link.container_path.is_relative(),
            )?;
            return target.read(&link.target_key).map_err(|error| {
                BrokenLinkError::new(key, format!("failed to read target entry: {error:#}")).into()
            });
        }
        self.local.read(key)
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
        self.save_metadata(&metadata)
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

    fn check_links(&self) -> Result<Vec<LinkCheckIssue>> {
        let metadata = self.outgoing_metadata()?;
        let root = self
            .metadata_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("link container has no root directory"))?;
        let mut issues = Vec::new();

        for (key, link) in &metadata.links {
            let path = root.join(key);
            if let Err(error) = validate_target(root, key, &self.uid, link) {
                issues.push(LinkCheckIssue {
                    key: key.clone(),
                    kind: LinkCheckKind::BrokenTarget,
                    expected: Some(resolved_container_path(root, link)),
                    actual: Some(PathBuf::from(error.to_string())),
                });
            }
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    issues.push(LinkCheckIssue {
                        key: key.clone(),
                        kind: LinkCheckKind::MissingSymlink,
                        expected: Some(symlink_target(
                            root,
                            &path,
                            link,
                            link.container_path.is_relative(),
                        )),
                        actual: None,
                    });
                }
                Err(error) => return Err(error.into()),
                Ok(file_type) if !file_type.is_symlink() => {
                    issues.push(LinkCheckIssue {
                        key: key.clone(),
                        kind: LinkCheckKind::IncorrectSymlink,
                        expected: Some(symlink_target(
                            root,
                            &path,
                            link,
                            link.container_path.is_relative(),
                        )),
                        actual: None,
                    });
                }
                Ok(_) => {
                    let actual = fs::read_link(&path)?;
                    let expected =
                        symlink_target(root, &path, link, link.container_path.is_relative());
                    if actual != expected {
                        issues.push(LinkCheckIssue {
                            key: key.clone(),
                            kind: LinkCheckKind::IncorrectSymlink,
                            expected: Some(expected),
                            actual: Some(actual),
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
                    key,
                    kind: LinkCheckKind::UnrecordedSymlink,
                    expected: None,
                    actual: Some(actual),
                });
            }
        }
        Ok(issues)
    }

    fn repair_link_issue(&self, issue: &LinkCheckIssue) -> Result<()> {
        let metadata = self.outgoing_metadata()?;
        let root = self
            .metadata_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("link container has no root directory"))?;
        let path = root.join(&issue.key);
        match issue.kind {
            LinkCheckKind::UnrecordedSymlink => {
                let file_type = fs::symlink_metadata(&path)?.file_type();
                if !file_type.is_symlink() {
                    bail!("{} is no longer a symlink", issue.key);
                }
                fs::remove_file(path)?;
            }
            LinkCheckKind::BrokenTarget => {
                let mut metadata = self.outgoing_metadata()?;
                metadata
                    .links
                    .remove(&issue.key)
                    .ok_or_else(|| anyhow!("link metadata disappeared: {}", issue.key))?;
                self.save_metadata(&metadata)?;
                if fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    fs::remove_file(path)?;
                }
            }
            LinkCheckKind::MissingSymlink | LinkCheckKind::IncorrectSymlink => {
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
        }
        Ok(())
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
        let target_filename = absolute_path(&target.filepath(target_key)?)?;
        if !target_filename.is_file() {
            return Err(LinkToError::TargetNotFound(target_key.clone()));
        }
        let target_uid = target.uid()?;
        let target_root = absolute_path(&target.root_path())?;
        let metadata = self.outgoing_metadata()?;
        let prefer_relative = prefer_relative.unwrap_or(metadata.prefer_relative);
        let container_path =
            recorded_container_path(&absolute_path(&self.root)?, &target_root, prefer_relative);
        let mut target_guard = target.writer()?;
        target_guard.link_from(&self.uid, target_key, linker_key)?;

        let result = self.install_outgoing_link(linker_key, target_key, target_uid, container_path);
        if result.is_err() {
            let _ = target_guard.unlink(&self.uid, target_key, linker_key);
        }
        result
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
        let (target, _) = validate_outgoing_link(
            &self.root,
            linker_key,
            &self.uid,
            &link_path,
            &link,
            link.container_path.is_relative(),
        )?;
        let mut target_guard = target.writer()?;
        target_guard.unlink(&self.uid, &link.target_key, linker_key)?;
        let result = self.remove_outgoing_link(linker_key);
        if result.is_err() {
            let _ = target_guard.link_from(&self.uid, &link.target_key, linker_key);
        }
        result
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
        let (mut target, _) = validate_outgoing_link(
            &self.root,
            from,
            &self.uid,
            &link_path,
            &link,
            link.container_path.is_relative(),
        )
        .map_err(LinkToError::Other)?;
        self.link_to(
            to,
            target.as_mut(),
            &link.target_key,
            Some(link.container_path.is_relative()),
        )
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
        validate_outgoing_link(
            &self.root,
            from,
            &self.uid,
            &self.entry_path(from)?,
            &link,
            link.container_path.is_relative(),
        )?;
        if self.outgoing_metadata()?.links.contains_key(to)
            || fs::symlink_metadata(self.entry_path(to)?).is_ok()
        {
            bail!("destination entry already exists: {to}");
        }
        self.unlink_to(from)
            .map_err(|error| anyhow!("failed to remove old link: {error}"))?;
        let target_path = resolved_container_path(&self.root, &link);
        let mut target = open_container(target_path)?;
        let preference = Some(link.container_path.is_relative());
        if let Err(error) = self.link_to(to, target.as_mut(), &link.target_key, preference) {
            let _ = self.link_to(from, target.as_mut(), &link.target_key, preference);
            return Err(anyhow!("failed to create renamed link: {error}"));
        }
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
            let _ = fs::remove_file(&link_path);
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
        if metadata.links.remove(linker_key).is_none() {
            return Err(UnlinkToError::LinkNotFound);
        }
        self.save_metadata(&metadata)?;
        match fs::remove_file(&link_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UnlinkToError::Other(error.into())),
        }
    }
}

impl ContainerWriteGuard for LinkContainerWriteGuard {
    fn container_uid(&self) -> &str {
        &self.uid
    }

    fn check(&mut self) -> Result<Vec<LinkCheckIssue>> {
        self.check_links()
    }

    fn repair_check(&mut self, issue: &LinkCheckIssue) -> Result<()> {
        self.repair_link_issue(issue)
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

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        debug!(entry_key = %key, "getting link information");
        let metadata = self.outgoing_metadata()?;
        if let Some(link) = metadata.links.get(key) {
            validate_outgoing_link(
                &self.root,
                key,
                &self.uid,
                &self.entry_path(key)?,
                link,
                link.container_path.is_relative(),
            )?;
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

    fn has_link_to(
        &self,
        linker_key: &EntryKey,
        target_uid: &str,
        target_key: &EntryKey,
    ) -> Result<bool> {
        Ok(self
            .outgoing_metadata()?
            .links
            .get(linker_key)
            .is_some_and(|link| link.container_uid == target_uid && link.target_key == *target_key))
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
    Other(#[from] anyhow::Error),
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
            version: CONTAINER_FORMAT_VERSION,
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
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata: OutgoingLinksMetadata = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if metadata.version != CONTAINER_FORMAT_VERSION {
        return Err(anyhow!(
            "unsupported format version {} in {}",
            metadata.version,
            path.display()
        ));
    }
    Ok(metadata)
}

fn validate_outgoing_link(
    root: &Path,
    key: &EntryKey,
    linker_uid: &str,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> Result<(Box<dyn Container>, PathBuf)> {
    let (container, target_path) = validate_target(root, key, linker_uid, link)?;
    verify_materialized_symlink(root, link_path, link, prefer_relative)
        .map_err(|error| BrokenLinkError::new(key, error.to_string()))?;
    Ok((container, target_path))
}

fn validate_target(
    root: &Path,
    key: &EntryKey,
    linker_uid: &str,
    link: &OutgoingLink,
) -> Result<(Box<dyn Container>, PathBuf)> {
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
    let target_path = container.filepath(&link.target_key).map_err(|error| {
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
    let target_guard = container.writer().map_err(|error| {
        BrokenLinkError::new(key, format!("failed to inspect target links: {error}"))
    })?;
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
    drop(target_guard);
    Ok((container, target_path))
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
