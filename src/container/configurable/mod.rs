//! Rule-routed Container storage built on validated LinkContainer semantics.

mod guard;
mod metadata;
mod pattern;
mod routing;
mod transaction;

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, trace};

use crate::logging::ContainerLogger;

use super::{
    Buffer, Container, ContainerEntryInfo, ContainerMetadata, ContainerWriteGuard, EntryKey,
    LinkContainer, WriterError,
};

pub use guard::ConfigurableContainerWriteGuard;
pub use metadata::{ConfigRule, ConfigRuleTarget};
pub use routing::{ForcedStorageWarning, RouteDecision, StorageClass};

use metadata::{initialize_rules, rules_path};

/// A link-capable Container whose ordinary writes are routed by ordered rules.
pub struct ConfigurableContainer {
    /// LinkContainer storage reused for files, links, validation, and locking.
    link: LinkContainer,
    /// Configurable-kind tracing identity.
    logger: ContainerLogger,
}

impl ConfigurableContainer {
    /// Opens or initializes a ConfigurableContainer with no routing rules.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem directory to open or create.
    ///
    /// # Returns
    ///
    /// A ConfigurableContainer whose new logical name comes from its directory name.
    ///
    /// # Errors
    ///
    /// Returns an error when common, outgoing, or rule metadata cannot be initialized or validated.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let logical_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("configurable")
            .to_owned();
        Self::with_logical_name(path, logical_name)
    }

    /// Opens or initializes a ConfigurableContainer with a caller-selected name.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem directory to open or create.
    /// * `logical_name` - Name persisted only for newly created common metadata.
    ///
    /// # Returns
    ///
    /// A ConfigurableContainer with existing rules preserved or empty rules initialized.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem, metadata, or concrete-kind failures.
    pub fn with_logical_name(
        path: impl Into<PathBuf>,
        logical_name: impl Into<String>,
    ) -> Result<Self> {
        let path = path.into();
        let link = LinkContainer::open_as(path.clone(), logical_name.into(), "configurable")?;
        initialize_rules(&rules_path(&path))?;
        Self::from_link(link)
    }

    /// Opens an existing ConfigurableContainer from parsed common metadata.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem root represented by `metadata`.
    /// * `metadata` - Common metadata required to declare kind `configurable`.
    ///
    /// # Returns
    ///
    /// A handle without reparsing common metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, rule metadata, or the root is invalid.
    pub fn from_metadata(path: impl Into<PathBuf>, metadata: ContainerMetadata) -> Result<Self> {
        let path = path.into();
        let link = LinkContainer::from_metadata_as(path.clone(), metadata, "configurable")?;
        metadata::load_rules(&rules_path(&path))?;
        Self::from_link(link)
    }

    /// Returns this Container's filesystem root.
    ///
    /// # Returns
    ///
    /// A borrowed root path.
    pub fn path(&self) -> &Path {
        self.link.path()
    }

    /// Replaces ordered routing rules after resolving and validating every target UID.
    ///
    /// # Arguments
    ///
    /// * `rules` - Complete first-match-wins replacement rule list.
    ///
    /// # Returns
    ///
    /// `Ok(())` after validation and synchronized persistence under this Container's lock.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid patterns/locators, unavailable targets,
    /// target UID mismatch, self-targeting, or persistence failure.
    pub fn set_rules(&self, rules: Vec<ConfigRule>) -> Result<()> {
        self.writer()?.set_rules(rules)
    }

    /// Returns the automatic storage preference for one key without writing data.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key to match against ordered rules.
    ///
    /// # Returns
    ///
    /// A local or link routing decision suitable for CLI preflight warnings.
    ///
    /// # Errors
    ///
    /// Returns an error when rules cannot be loaded or a pattern is invalid.
    pub fn route_decision(&self, key: &EntryKey) -> Result<RouteDecision> {
        routing::decide(
            self.path(),
            &metadata::load_rules(&rules_path(self.path()))?,
            key,
        )
    }

    /// Describes whether a forced storage operation contradicts automatic routing.
    ///
    /// # Arguments
    ///
    /// * `key` - Entry key affected by a forced local/link operation.
    /// * `forced` - Storage class selected explicitly by the caller.
    ///
    /// # Returns
    ///
    /// `Some` when a later ordinary write would select another class; otherwise `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when rule loading or matching fails.
    pub fn forced_storage_warning(
        &self,
        key: &EntryKey,
        forced: StorageClass,
    ) -> Result<Option<ForcedStorageWarning>> {
        let automatic = self.route_decision(key)?;
        let needs_warning = forced == StorageClass::Link || automatic.storage_class() != forced;
        Ok(needs_warning.then(|| ForcedStorageWarning {
            key: key.clone(),
            forced,
            automatic,
        }))
    }

    /// Acquires the concrete configurable writer.
    ///
    /// # Returns
    ///
    /// A guard initially owning this Container's exclusive lock.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::ContainerLocked`] on contention or another setup error.
    pub fn writer(&self) -> std::result::Result<ConfigurableContainerWriteGuard, WriterError> {
        Ok(ConfigurableContainerWriteGuard::new(
            self.path().to_owned(),
            self.link.writer()?,
            self.logger.clone(),
        ))
    }

    /// Creates a configurable handle from validated link-capable storage.
    ///
    /// # Arguments
    ///
    /// * `link` - Link-capable storage whose common kind is `configurable`.
    ///
    /// # Returns
    ///
    /// A handle with tracing identity derived from the shared UID.
    ///
    /// # Errors
    ///
    /// Returns an error when the UID cannot be read.
    fn from_link(link: LinkContainer) -> Result<Self> {
        let logger = ContainerLogger::new("configurable", link.uid()?);
        debug!(path = %link.path().display(), container_uid = %logger.uid(), "opened configurable container");
        Ok(Self { link, logger })
    }

    /// Lists ordinary local entries only.
    ///
    /// # Returns
    ///
    /// Stable local entry keys from underlying link-capable storage.
    ///
    /// # Errors
    ///
    /// Returns an error when filesystem traversal fails.
    pub fn local_list(&self) -> Result<Vec<EntryKey>> {
        self.link.local_list()
    }

    /// Lists only outgoing link keys.
    ///
    /// # Returns
    ///
    /// Stable keys from authoritative outgoing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata cannot be read or migrated.
    pub fn link_list(&self) -> Result<Vec<EntryKey>> {
        self.link.link_list()
    }

    /// Lists local entries with structured link information.
    ///
    /// # Returns
    ///
    /// One record per ordinary local entry.
    ///
    /// # Errors
    ///
    /// Returns an error for listing, lock, or metadata failures.
    pub fn local_list_info(&self) -> Result<Vec<ContainerEntryInfo>> {
        self.link.local_list_info()
    }

    /// Lists outgoing entries with structured target information.
    ///
    /// # Returns
    ///
    /// One record per outgoing key.
    ///
    /// # Errors
    ///
    /// Returns an error for listing, lock, or metadata failures.
    pub fn link_list_info(&self) -> Result<Vec<ContainerEntryInfo>> {
        self.link.link_list_info()
    }

    /// Returns the default relative-path policy inherited from link-capable storage.
    ///
    /// # Returns
    ///
    /// `true` when newly created links prefer relative target paths.
    ///
    /// # Errors
    ///
    /// Returns an error when outgoing metadata cannot be read or locked.
    pub fn prefer_relative(&self) -> Result<bool> {
        self.link.prefer_relative()
    }

    /// Reads one ordinary local entry without following an outgoing link.
    ///
    /// # Arguments
    ///
    /// * `key` - Local entry key.
    ///
    /// # Returns
    ///
    /// Newly allocated local file bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/outgoing keys, locking, or reading failures.
    pub fn local_read(&self, key: &EntryKey) -> Result<Buffer> {
        self.link.local_read(key)
    }

    /// Resolves one ordinary local entry path without following outgoing links.
    ///
    /// # Arguments
    ///
    /// * `key` - Local entry key.
    ///
    /// # Returns
    ///
    /// The root-relative filesystem path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/outgoing keys or metadata/lock failures.
    pub fn local_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.link.local_filepath(key)
    }
}

impl Container for ConfigurableContainer {
    fn metadata(&self) -> ContainerMetadata {
        self.link.metadata()
    }

    fn uid(&self) -> Result<String> {
        self.link.uid()
    }

    fn logical_name(&self) -> String {
        self.link.logical_name()
    }

    fn kind(&self) -> String {
        trace!("getting configurable container kind");
        "configurable".to_owned()
    }

    fn root_path(&self) -> PathBuf {
        self.path().to_owned()
    }

    fn list(&self) -> Result<Vec<EntryKey>> {
        self.link.list()
    }

    fn list_info(&self) -> Result<Vec<ContainerEntryInfo>> {
        self.link.list_info()
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.link.filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        self.link.read(key)
    }

    fn writer(&self) -> std::result::Result<Box<dyn ContainerWriteGuard>, WriterError> {
        Ok(Box::new(self.writer()?))
    }
}

#[cfg(test)]
mod tests;
