//! Minecraft-root provider, metadata initialization, and logical catalog nodes.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::container::{CONTAINER_METADATA_FILE, CONTROL_DIR, Container, LocalContainer};
use crate::destination::provider::{
    DESTINATION_FORMAT_VERSION, DESTINATION_METADATA_FILE, DestinationProvider,
};
use crate::destination::{ContainerDescriptor, Destination, Subcontainer, SubcontainerDescriptor};

use super::catalog::{ManagedNode, node_containers, node_subcontainers, open_node_container};
use super::version::{Version, discover_versions, open_version};

/// Stable provider kind persisted in `.kcl/destination.json`.
pub(crate) const MINECRAFT_KIND: &str = "minecraft";

/// A Destination rooted at one Minecraft installation directory.
#[derive(Debug, Clone)]
pub struct McDestination {
    /// Filesystem root of the Minecraft installation.
    root: PathBuf,
}

impl McDestination {
    /// Opens or initializes a Minecraft Destination at `root`.
    ///
    /// # Arguments
    ///
    /// * `root` - Minecraft installation directory, commonly `~/.minecraft`.
    ///
    /// # Returns
    ///
    /// An initialized Minecraft Destination. The managed `mod-cache` LocalContainer
    /// is created on demand at `<root>/.kcl/mod-cache`.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be created or inspected, existing
    /// Destination metadata has another kind, or mod-cache initialization fails.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create Minecraft root {}", root.display()))?;
        ensure_metadata(&root)?;
        let destination = Self { root };
        destination.ensure_mod_cache()?;
        Ok(destination)
    }

    /// Opens an existing Minecraft Destination selected by provider metadata.
    ///
    /// # Arguments
    ///
    /// * `root` - Existing root containing `.kcl/destination.json` with kind `minecraft`.
    ///
    /// # Returns
    ///
    /// An opened Destination with the managed mod-cache Container initialized.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is missing, metadata is invalid or has a
    /// different kind, or mod-cache cannot be opened as a LocalContainer.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let metadata = crate::destination::DestinationMetadata::load(&root)?;
        if metadata.kind != MINECRAFT_KIND {
            bail!(
                "Destination at {} has kind '{}', expected '{}'",
                root.display(),
                metadata.kind,
                MINECRAFT_KIND
            );
        }
        if !root.is_dir() {
            bail!("Minecraft root is not a directory: {}", root.display());
        }
        let destination = Self { root };
        destination.ensure_mod_cache()?;
        Ok(destination)
    }

    /// Returns the Minecraft installation filesystem root.
    ///
    /// # Returns
    ///
    /// A borrowed path valid for this Destination handle's lifetime.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Opens every valid directly discovered Version in stable ID order.
    ///
    /// # Returns
    ///
    /// Versions whose directory contains the required `<id>.json` manifest.
    /// Invalid candidates and unrelated directories are omitted.
    ///
    /// # Errors
    ///
    /// Returns an error when the `versions` directory or one of its entries
    /// cannot be inspected, or a valid Version cannot initialize.
    pub fn versions(&self) -> Result<Vec<Version>> {
        discover_versions(self)
    }

    /// Returns the shared content-addressed mod-cache filesystem path.
    ///
    /// # Returns
    ///
    /// `<root>/.kcl/mod-cache` without opening the Container.
    pub fn mod_cache_path(&self) -> PathBuf {
        self.root.join(CONTROL_DIR).join("mod-cache")
    }

    /// Ensures the shared mod-cache is a LocalContainer and returns no handle.
    fn ensure_mod_cache(&self) -> Result<()> {
        let cache = LocalContainer::new(self.mod_cache_path())?;
        if cache.kind() != "local" {
            bail!("Minecraft mod-cache is not a local Container");
        }
        Ok(())
    }
}

impl Subcontainer for McDestination {
    fn boxed_clone(&self) -> Box<dyn Subcontainer> {
        Box::new(self.clone())
    }

    fn containers(&self) -> Result<Vec<ContainerDescriptor>> {
        node_containers(&self.root, ManagedNode::Root)
    }

    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>> {
        node_subcontainers(&self.root, ManagedNode::Root)
    }

    fn open_container(&self, name: &str) -> Result<Box<dyn Container>> {
        open_node_container(&self.root, ManagedNode::Root, name)
    }

    fn open_subcontainer(&self, name: &str) -> Result<Box<dyn Subcontainer>> {
        let node = match name {
            "assets" => ManagedNode::Assets,
            "versions" => ManagedNode::Versions,
            _ => bail!("Minecraft Subcontainer is not managed: {name}"),
        };
        Ok(Box::new(MinecraftSubcontainer {
            root: self.root.clone(),
            node,
        }))
    }
}

impl Destination for McDestination {
    fn root_path(&self) -> &Path {
        &self.root
    }

    fn kind(&self) -> &str {
        MINECRAFT_KIND
    }
}

/// Provider registered by the standard Destination registry for Minecraft roots.
#[derive(Debug, Clone, Copy, Default)]
pub struct McDestinationProvider;

impl DestinationProvider for McDestinationProvider {
    fn kind(&self) -> &'static str {
        MINECRAFT_KIND
    }

    fn open(&self, root: &Path) -> Result<Box<dyn Destination>> {
        Ok(Box::new(McDestination::open(root)?))
    }
}

/// A logical managed Subcontainer below a Minecraft root.
#[derive(Debug, Clone)]
pub(crate) struct MinecraftSubcontainer {
    /// Minecraft installation root.
    root: PathBuf,
    /// Fixed or dynamic managed node represented by this handle.
    node: ManagedNode,
}

impl Subcontainer for MinecraftSubcontainer {
    fn boxed_clone(&self) -> Box<dyn Subcontainer> {
        Box::new(self.clone())
    }

    fn containers(&self) -> Result<Vec<ContainerDescriptor>> {
        node_containers(&self.root, self.node)
    }

    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>> {
        node_subcontainers(&self.root, self.node)
    }

    fn open_container(&self, name: &str) -> Result<Box<dyn Container>> {
        open_node_container(&self.root, self.node, name)
    }

    fn open_subcontainer(&self, _name: &str) -> Result<Box<dyn Subcontainer>> {
        bail!("Minecraft managed Subcontainer has no nested Subcontainers")
    }
}

/// Metadata written to a Minecraft root's common Destination file.
#[derive(Serialize)]
struct DestinationMetadataFile {
    /// Common Destination schema version.
    version: u32,
    /// Provider kind used by the registry.
    kind: &'static str,
}

/// Creates or validates common Minecraft Destination metadata.
fn ensure_metadata(root: &Path) -> Result<()> {
    let control = root.join(CONTROL_DIR);
    fs::create_dir_all(&control)?;
    let path = control.join(DESTINATION_METADATA_FILE);
    if path.is_file() {
        let metadata = crate::destination::DestinationMetadata::load(root)?;
        if metadata.kind != MINECRAFT_KIND {
            bail!(
                "Destination at {} has kind '{}', expected '{}'",
                root.display(),
                metadata.kind,
                MINECRAFT_KIND
            );
        }
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    serde_json::to_writer_pretty(
        &mut file,
        &DestinationMetadataFile {
            version: DESTINATION_FORMAT_VERSION,
            kind: MINECRAFT_KIND,
        },
    )?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Reports whether a path names one safe Minecraft directory component.
pub(crate) fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value != CONTROL_DIR
        && !value.contains(':')
        && !value.contains('/')
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .eq([Component::Normal(std::ffi::OsStr::new(value))])
}

/// Returns whether a managed Container has initialized common metadata.
pub(crate) fn is_initialized(path: &Path) -> bool {
    path.join(CONTROL_DIR)
        .join(CONTAINER_METADATA_FILE)
        .is_file()
}

/// Returns a sorted set of safe version directory names.
pub(crate) fn version_names(root: &Path) -> Result<BTreeSet<String>> {
    let versions = root.join("versions");
    if !versions.exists() {
        return Ok(BTreeSet::new());
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&versions)
        .with_context(|| format!("failed to list Minecraft versions {}", versions.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_safe_component(&name) && entry.path().join(format!("{name}.json")).is_file() {
            names.insert(name);
        }
    }
    Ok(names)
}

/// Converts a fixed or dynamic node to its filesystem root.
pub(crate) fn node_path(root: &Path, node: ManagedNode, name: &str) -> Result<PathBuf> {
    if !is_safe_component(name) {
        return Err(anyhow!("invalid Minecraft managed member name: {name:?}"));
    }
    Ok(match node {
        ManagedNode::Root => match name {
            "libraries" => root.join("libraries"),
            "mod-cache" => root.join(CONTROL_DIR).join("mod-cache"),
            _ => bail!("Minecraft root Container is not managed: {name}"),
        },
        ManagedNode::Assets => match name {
            "objects" => root.join("assets/objects"),
            "indexes" => root.join("assets/indexes"),
            _ => bail!("Minecraft assets Container is not managed: {name}"),
        },
        ManagedNode::Versions => {
            if !version_names(root)?.contains(name) {
                bail!("Minecraft Version is not valid or not discovered: {name}");
            }
            root.join("versions").join(name)
        }
    })
}

/// Opens a dynamic Version Container from its managed node.
pub(crate) fn open_managed_version(root: &Path, name: &str) -> Result<Box<dyn Container>> {
    Ok(Box::new(open_version(root, name)?))
}
