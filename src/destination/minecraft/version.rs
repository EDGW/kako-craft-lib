//! Minecraft Version discovery and ConfigurableContainer wrapping.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::container::EntryKey;
use crate::container::{
    ConfigRule, ConfigurableContainer, Container, ContainerMetadata, LocalContainer,
};
use crate::destination::minecraft::destination::{McDestination, is_safe_component, version_names};
use crate::locator::ContainerLocator;

/// A valid Minecraft Version directory represented as one ConfigurableContainer.
pub struct Version {
    /// Minecraft version identifier and directory name.
    id: String,
    /// Underlying ConfigurableContainer for the whole Version directory.
    container: ConfigurableContainer,
}

impl Version {
    /// Returns the Minecraft version identifier.
    ///
    /// # Returns
    ///
    /// The manifest-matching directory name, such as `1.20.1`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the Version directory path.
    ///
    /// # Returns
    ///
    /// `<minecraft-root>/versions/<id>`.
    pub fn path(&self) -> &Path {
        self.container.path()
    }

    /// Returns the Version manifest path.
    ///
    /// # Returns
    ///
    /// `<version-path>/<id>.json`.
    pub fn manifest_path(&self) -> PathBuf {
        self.path().join(format!("{}.json", self.id))
    }

    /// Returns the underlying ConfigurableContainer handle.
    ///
    /// # Returns
    ///
    /// A borrowed container containing every Version file and directory entry.
    pub fn container(&self) -> &ConfigurableContainer {
        &self.container
    }

    /// Opens an already discovered Version and applies its default mod routing
    /// only when no user rule document existed.
    pub(crate) fn open(root: &Path, id: &str) -> Result<Self> {
        if !is_safe_component(id) {
            bail!("invalid Minecraft Version id: {id:?}");
        }
        let version_root = root.join("versions").join(id);
        let manifest = version_root.join(format!("{id}.json"));
        if !version_root.is_dir() || !manifest.is_file() {
            bail!(
                "Minecraft Version manifest is missing: {}",
                manifest.display()
            );
        }
        let rules_path = version_root
            .join(crate::container::CONTROL_DIR)
            .join("link-matches.json");
        let had_rules = rules_path.is_file();
        let container = ConfigurableContainer::new(version_root)?;
        if had_rules {
            container.validate_rule_targets()?;
        } else {
            let cache =
                LocalContainer::new(root.join(crate::container::CONTROL_DIR).join("mod-cache"))?;
            let locator: ContainerLocator = "../../:mod-cache".parse()?;
            container.set_rules(vec![ConfigRule::sha1("/mods/**", locator, cache.uid()?)])?;
        }
        Ok(Self {
            id: id.to_owned(),
            container,
        })
    }
}

impl Container for Version {
    fn metadata(&self) -> ContainerMetadata {
        self.container.metadata()
    }

    fn uid(&self) -> Result<String> {
        self.container.uid()
    }

    fn logical_name(&self) -> String {
        self.container.logical_name()
    }

    fn kind(&self) -> String {
        self.container.kind()
    }

    fn root_path(&self) -> PathBuf {
        self.container.root_path()
    }

    fn list(&self) -> Result<Vec<String>> {
        self.container.list()
    }

    fn list_info(&self) -> Result<Vec<crate::container::ContainerEntryInfo>> {
        self.container.list_info()
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.container.filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Vec<u8>> {
        self.container.read(key)
    }

    fn writer(
        &self,
    ) -> std::result::Result<
        Box<dyn crate::container::ContainerWriteGuard>,
        crate::container::WriterError,
    > {
        self.container
            .writer()
            .map(|guard| Box::new(guard) as Box<dyn crate::container::ContainerWriteGuard>)
    }
}

/// Discovers valid Versions below a Minecraft Destination.
pub(crate) fn discover_versions(destination: &McDestination) -> Result<Vec<Version>> {
    version_names(destination.path())?
        .into_iter()
        .map(|id| open_version(destination.path(), &id))
        .collect()
}

/// Opens one valid Version by ID.
pub(crate) fn open_version(root: &Path, id: &str) -> Result<Version> {
    Version::open(root, id).with_context(|| format!("failed to open Minecraft Version {id}"))
}
