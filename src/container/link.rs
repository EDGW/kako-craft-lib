use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

use crate::logging::ContainerLogger;

use super::{
    Buffer, CONTAINER_FORMAT_VERSION, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, EntryKey, LinkFromError, LinkInfo, LocalContainer, UnlinkError,
    WriteError, WriterError,
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

    /// Links `linker_key` in this container to `target_key` in `target`.
    ///
    /// When `prefer_relative` is true, the materialized symlink uses a relative
    /// target whenever a relative path can be computed. The preference and the
    /// absolute target filename are both persisted in metadata.
    pub fn link_to(
        &mut self,
        linker_key: &EntryKey,
        target: &mut dyn Container,
        target_key: &EntryKey,
        prefer_relative: bool,
    ) -> std::result::Result<(), LinkToError> {
        let span = container_operation_span!(self.logger, "link_to");
        let _entered = span.enter();
        debug!(
            linker_key = %linker_key,
            target_key = %target_key,
            prefer_relative,
            "creating outgoing link"
        );
        let target_filename = absolute_path(&target.filepath(target_key)?)?;
        if !target_filename.is_file() {
            return Err(LinkToError::TargetNotFound(target_key.clone()));
        }
        let target_uid = target.uid()?;

        // Lock the target while registering its reciprocal LinkFrom record.
        let mut target_guard = target.writer()?;
        target_guard.link_from(self, target_key, linker_key)?;

        let result = self.install_outgoing_link(
            linker_key,
            target_key,
            target_uid,
            target_filename,
            prefer_relative,
        );
        if result.is_err() {
            // Best-effort rollback keeps the two metadata stores consistent.
            debug!(linker_key = %linker_key, target_key = %target_key, "rolling back incoming link registration");
            let _ = target_guard.unlink(self, target_key, linker_key);
        }
        result
    }

    /// Removes an outgoing link and its reciprocal LinkFrom record.
    pub fn unlink_to(
        &mut self,
        linker_key: &EntryKey,
        target: &mut dyn Container,
    ) -> std::result::Result<(), UnlinkToError> {
        let span = container_operation_span!(self.logger, "unlink_to");
        let _entered = span.enter();
        debug!(linker_key = %linker_key, "removing outgoing link");
        let metadata = self.outgoing_metadata()?;
        let link = metadata
            .links
            .get(linker_key)
            .cloned()
            .ok_or(UnlinkToError::LinkNotFound)?;
        if link.container_uid != target.uid()? {
            return Err(UnlinkToError::ContainerMismatch);
        }

        let mut target_guard = target.writer()?;
        target_guard.unlink(self, &link.target_key, linker_key)?;

        let result = self.remove_outgoing_link(linker_key);
        if result.is_err() {
            debug!(linker_key = %linker_key, target_key = %link.target_key, "rolling back incoming link removal");
            let _ = target_guard.link_from(self, &link.target_key, linker_key);
        }
        result
    }

    fn outgoing_metadata_path(&self) -> PathBuf {
        self.local.path_ref().join(CONTROL_DIR).join(METADATA_FILE)
    }

    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        read_metadata(&self.outgoing_metadata_path())
    }

    fn guarded(&self) -> std::result::Result<LinkContainerWriteGuard, WriterError> {
        Ok(LinkContainerWriteGuard {
            metadata_path: self.outgoing_metadata_path(),
            local: self.local.writer()?,
            logger: self.logger.clone(),
        })
    }

    fn install_outgoing_link(
        &self,
        linker_key: &EntryKey,
        target_key: &EntryKey,
        container_uid: String,
        target_filename: PathBuf,
        prefer_relative: bool,
    ) -> std::result::Result<(), LinkToError> {
        let guard = self.guarded()?;
        let link_path = self.local.filepath(linker_key)?;
        let mut metadata = guard.outgoing_metadata()?;

        if let Some(existing) = metadata.links.get(linker_key) {
            return if existing.container_uid == container_uid
                && existing.target_key == *target_key
                && existing.target_filename == target_filename
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
            target_filename,
            prefer_relative,
        };
        materialize_symlink(&link_path, &link)?;
        metadata.links.insert(linker_key.clone(), link);
        if let Err(error) = guard.save_metadata(&metadata) {
            let _ = fs::remove_file(&link_path);
            return Err(error.into());
        }
        Ok(())
    }

    fn remove_outgoing_link(
        &self,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkToError> {
        let guard = self.guarded()?;
        let link_path = self.local.filepath(linker_key)?;
        let mut metadata = guard.outgoing_metadata()?;
        if metadata.links.remove(linker_key).is_none() {
            return Err(UnlinkToError::LinkNotFound);
        }
        guard.save_metadata(&metadata)?;

        match fs::remove_file(&link_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UnlinkToError::Other(error.into())),
        }
    }
}

impl Container for LinkContainer {
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
        keys.extend(self.outgoing_metadata()?.links.into_keys());
        debug!(entry_count = keys.len(), "listed container entries");
        Ok(keys.into_iter().collect())
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        let span = container_operation_span!(self.logger, "filepath");
        let _entered = span.enter();
        debug!(entry_key = %key, "resolving entry path");
        let path = self.local.filepath(key)?;
        if let Some(link) = self.outgoing_metadata()?.links.get(key) {
            verify_materialized_symlink(&path, link)?;
        }
        Ok(path)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        let span = container_operation_span!(self.logger, "read");
        let _entered = span.enter();
        debug!(entry_key = %key, "reading container entry");
        if let Some(link) = self.outgoing_metadata()?.links.get(key) {
            // Metadata, rather than the symlink, chooses the target.
            return fs::read(&link.target_filename).with_context(|| {
                format!(
                    "failed to read linked entry {} from {}",
                    key,
                    link.target_filename.display()
                )
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

struct LinkContainerWriteGuard {
    metadata_path: PathBuf,
    local: Box<dyn ContainerWriteGuard>,
    logger: ContainerLogger,
}

impl LinkContainerWriteGuard {
    fn outgoing_metadata(&self) -> Result<OutgoingLinksMetadata> {
        read_metadata(&self.metadata_path)
    }

    fn save_metadata(&self, metadata: &OutgoingLinksMetadata) -> Result<()> {
        write_json_atomic(&self.metadata_path, metadata)
    }
}

impl ContainerWriteGuard for LinkContainerWriteGuard {
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

    fn link_from(
        &mut self,
        linker: &mut dyn Container,
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
        self.local.link_from(linker, target_key, linker_key)
    }

    fn unlink(
        &mut self,
        linker: &mut dyn Container,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError> {
        let span = container_operation_span!(self.logger, "unlink");
        let _entered = span.enter();
        debug!(target_key = %target_key, linker_key = %linker_key, "removing incoming link");
        self.local.unlink(linker, target_key, linker_key)
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        let span = container_operation_span!(self.logger, "link_info");
        let _entered = span.enter();
        debug!(entry_key = %key, "getting link information");
        if let Some(link) = self.outgoing_metadata()?.links.get(key) {
            return Ok(LinkInfo::LinkTo {
                target_key: link.target_key.clone(),
                container_uid: link.container_uid.clone(),
            });
        }
        self.local.link_info(key)
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

#[derive(Debug, Serialize, Deserialize)]
struct OutgoingLinksMetadata {
    version: u32,
    links: BTreeMap<EntryKey, OutgoingLink>,
}

impl Default for OutgoingLinksMetadata {
    fn default() -> Self {
        Self {
            version: CONTAINER_FORMAT_VERSION,
            links: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutgoingLink {
    target_key: EntryKey,
    container_uid: String,
    target_filename: PathBuf,
    prefer_relative: bool,
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

fn symlink_target(link_path: &Path, link: &OutgoingLink) -> PathBuf {
    if link.prefer_relative
        && let Some(parent) = link_path.parent()
        && let Some(relative) = pathdiff::diff_paths(&link.target_filename, parent)
    {
        return relative;
    }
    link.target_filename.clone()
}

fn materialize_symlink(link_path: &Path, link: &OutgoingLink) -> Result<()> {
    let parent = link_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", link_path.display()))?;
    fs::create_dir_all(parent)?;
    let target = symlink_target(link_path, link);
    create_file_symlink(&target, link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

fn verify_materialized_symlink(link_path: &Path, link: &OutgoingLink) -> Result<()> {
    let actual = fs::read_link(link_path).with_context(|| {
        format!(
            "outgoing link metadata exists but {} is not a symlink",
            link_path.display()
        )
    })?;
    let expected = symlink_target(link_path, link);
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

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
