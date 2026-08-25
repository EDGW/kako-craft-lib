//! Catalog mutation, node enumeration, and Container initialization.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tracing::warn;

use crate::container::{
    CONTAINER_METADATA_FILE, CONTROL_DIR, ConfigurableContainer, Container, LinkContainer,
    LocalContainer, open_container,
};
use crate::destination::{
    ContainerDescriptor, Destination, DestinationProvider, Subcontainer, SubcontainerDescriptor,
};

use super::metadata::{
    CATALOG_KIND, CatalogContainer, initialize, load, save, validate_logical_path,
};

/// Filesystem-backed Destination whose members come only from explicit metadata.
#[derive(Debug, Clone)]
pub struct CatalogDestination {
    /// Destination filesystem root.
    root: PathBuf,
}

impl CatalogDestination {
    /// Opens or initializes an empty explicit catalog Destination.
    ///
    /// # Arguments
    ///
    /// * `root` - Filesystem directory that owns catalog metadata and members.
    ///
    /// # Returns
    ///
    /// A validated catalog Destination handle.
    ///
    /// # Errors
    ///
    /// Returns an error when root or metadata creation/loading fails.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        initialize(&root)?;
        load(&root)?;
        Ok(Self { root })
    }

    /// Opens an existing explicit catalog Destination.
    ///
    /// # Arguments
    ///
    /// * `root` - Existing root containing catalog metadata.
    ///
    /// # Returns
    ///
    /// A validated catalog Destination handle without adding members.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata is absent, invalid, or has another kind.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        load(&root)?;
        Ok(Self { root })
    }

    /// Declares a logical Subcontainer, creating missing ancestors.
    ///
    /// # Arguments
    ///
    /// * `logical_path` - Slash-separated path from the Destination root.
    /// * `force` - Whether to allow a Container with the same final path.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the updated catalog is persisted.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, a non-forced same-name Container,
    /// or metadata persistence failure.
    pub fn add_subcontainer(&self, logical_path: &str, force: bool) -> Result<()> {
        validate_logical_path(logical_path)?;
        let mut metadata = load(&self.root)?;
        if metadata.containers.contains_key(logical_path) && !force {
            bail!("a Container already uses logical path {logical_path}");
        }
        insert_parent_subcontainers(&mut metadata.subcontainers, logical_path);
        metadata.subcontainers.insert(logical_path.to_owned());
        save(&self.root, &metadata)
    }

    /// Declares one Container and any missing ancestor Subcontainers.
    ///
    /// # Arguments
    ///
    /// * `logical_path` - Slash-separated logical Container path.
    /// * `kind` - Supported concrete kind: `local`, `link`, or `configurable`.
    /// * `filesystem_path` - Destination-root-relative or absolute storage path;
    ///   `None` uses `logical_path`.
    /// * `force` - Whether to allow a Subcontainer with the same final path.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the explicit descriptor is persisted.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths/kinds, a non-forced same-name
    /// Subcontainer, an existing different descriptor, or persistence failure.
    pub fn add_container(
        &self,
        logical_path: &str,
        kind: &str,
        filesystem_path: Option<PathBuf>,
        force: bool,
    ) -> Result<()> {
        validate_logical_path(logical_path)?;
        validate_container_kind(kind)?;
        let mut metadata = load(&self.root)?;
        if metadata.subcontainers.contains(logical_path) && !force {
            bail!("a Subcontainer already uses logical path {logical_path}");
        }
        let specification = CatalogContainer {
            kind: kind.to_owned(),
            filesystem_path: filesystem_path.unwrap_or_else(|| PathBuf::from(logical_path)),
        };
        if let Some(existing) = metadata.containers.get(logical_path)
            && (existing.kind != specification.kind
                || existing.filesystem_path != specification.filesystem_path)
        {
            bail!("a different Container descriptor already uses {logical_path}");
        }
        insert_parent_subcontainers(&mut metadata.subcontainers, logical_path);
        metadata
            .containers
            .insert(logical_path.to_owned(), specification);
        save(&self.root, &metadata)
    }

    /// Creates a handle for one logical node within this catalog.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Empty root prefix or an existing Subcontainer path.
    ///
    /// # Returns
    ///
    /// A lightweight node retaining this Destination root.
    fn node(&self, prefix: impl Into<String>) -> CatalogNode {
        CatalogNode {
            root: self.root.clone(),
            prefix: prefix.into(),
        }
    }
}

impl Subcontainer for CatalogDestination {
    fn boxed_clone(&self) -> Box<dyn Subcontainer> {
        Box::new(self.clone())
    }

    fn containers(&self) -> Result<Vec<ContainerDescriptor>> {
        self.node("").containers()
    }

    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>> {
        self.node("").subcontainers()
    }

    fn open_container(&self, name: &str) -> Result<Box<dyn Container>> {
        self.node("").open_container(name)
    }

    fn open_subcontainer(&self, name: &str) -> Result<Box<dyn Subcontainer>> {
        self.node("").open_subcontainer(name)
    }
}

impl Destination for CatalogDestination {
    fn root_path(&self) -> &Path {
        &self.root
    }

    fn kind(&self) -> &str {
        CATALOG_KIND
    }
}

/// Provider registered for explicit catalog Destinations.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatalogDestinationProvider;

impl DestinationProvider for CatalogDestinationProvider {
    fn kind(&self) -> &'static str {
        CATALOG_KIND
    }

    fn open(&self, root: &Path) -> Result<Box<dyn Destination>> {
        Ok(Box::new(CatalogDestination::open(root)?))
    }
}

/// One logical Subcontainer node inside a catalog Destination.
#[derive(Debug, Clone)]
struct CatalogNode {
    /// Destination filesystem root.
    root: PathBuf,
    /// Empty root prefix or slash-separated Subcontainer path.
    prefix: String,
}

impl CatalogNode {
    /// Joins a direct child name to this node prefix.
    ///
    /// # Arguments
    ///
    /// * `name` - Valid direct catalog member name.
    ///
    /// # Returns
    ///
    /// A full logical path below the Destination root.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not exactly one valid component.
    fn child_path(&self, name: &str) -> Result<String> {
        validate_logical_path(name)?;
        if name.contains('/') {
            bail!("expected a direct member name, found {name:?}");
        }
        Ok(if self.prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{}", self.prefix, name)
        })
    }

    /// Resolves a persisted member filesystem path.
    ///
    /// # Arguments
    ///
    /// * `path` - Absolute or Destination-root-relative path from metadata.
    ///
    /// # Returns
    ///
    /// An absolute input unchanged, otherwise `root.join(path)`.
    fn filesystem_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        }
    }
}

impl Subcontainer for CatalogNode {
    fn boxed_clone(&self) -> Box<dyn Subcontainer> {
        Box::new(self.clone())
    }

    fn containers(&self) -> Result<Vec<ContainerDescriptor>> {
        let metadata = load(&self.root)?;
        warn_on_same_name(
            &metadata.containers.keys().cloned().collect(),
            &metadata.subcontainers,
        );
        let mut descriptors = metadata
            .containers
            .iter()
            .filter(|(path, _)| parent_path(path) == self.prefix)
            .map(|(logical_path, specification)| {
                let filesystem_path = self.filesystem_path(&specification.filesystem_path);
                ContainerDescriptor {
                    name: final_name(logical_path).to_owned(),
                    logical_path: logical_path.clone(),
                    kind: specification.kind.clone(),
                    initialized: filesystem_path
                        .join(CONTROL_DIR)
                        .join(CONTAINER_METADATA_FILE)
                        .is_file(),
                    filesystem_path,
                }
            })
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Ok(descriptors)
    }

    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>> {
        let metadata = load(&self.root)?;
        warn_on_same_name(
            &metadata.containers.keys().cloned().collect(),
            &metadata.subcontainers,
        );
        Ok(metadata
            .subcontainers
            .iter()
            .filter(|path| parent_path(path) == self.prefix)
            .map(|logical_path| SubcontainerDescriptor {
                name: final_name(logical_path).to_owned(),
                logical_path: logical_path.clone(),
            })
            .collect())
    }

    fn open_container(&self, name: &str) -> Result<Box<dyn Container>> {
        let logical_path = self.child_path(name)?;
        let metadata = load(&self.root)?;
        let specification = metadata
            .containers
            .get(&logical_path)
            .ok_or_else(|| anyhow!("Container is not declared: {logical_path}"))?;
        let path = self.filesystem_path(&specification.filesystem_path);
        if path
            .join(CONTROL_DIR)
            .join(CONTAINER_METADATA_FILE)
            .is_file()
        {
            let container = open_container(&path)?;
            if container.kind() != specification.kind {
                bail!(
                    "catalog expects '{}' at {}, found '{}'",
                    specification.kind,
                    path.display(),
                    container.kind()
                );
            }
            return Ok(container);
        }
        initialize_container(&path, &logical_path, &specification.kind)
            .with_context(|| format!("failed to initialize catalog Container {logical_path}"))
    }

    fn open_subcontainer(&self, name: &str) -> Result<Box<dyn Subcontainer>> {
        let logical_path = self.child_path(name)?;
        let metadata = load(&self.root)?;
        if !metadata.subcontainers.contains(&logical_path) {
            bail!("Subcontainer is not declared: {logical_path}");
        }
        Ok(Box::new(Self {
            root: self.root.clone(),
            prefix: logical_path,
        }))
    }
}

/// Initializes one supported concrete Container kind.
///
/// # Arguments
///
/// * `path` - Filesystem root to initialize.
/// * `logical_name` - Logical catalog path used as the initial Container name.
/// * `kind` - Concrete kind selected by the descriptor.
///
/// # Returns
///
/// A boxed initialized Container.
///
/// # Errors
///
/// Returns an error for unsupported kinds or implementation initialization failures.
fn initialize_container(path: &Path, logical_name: &str, kind: &str) -> Result<Box<dyn Container>> {
    match kind {
        "local" => Ok(Box::new(LocalContainer::with_logical_name(
            path,
            logical_name,
        )?)),
        "link" => Ok(Box::new(LinkContainer::with_logical_name(
            path,
            logical_name,
        )?)),
        "configurable" => Ok(Box::new(ConfigurableContainer::with_logical_name(
            path,
            logical_name,
        )?)),
        _ => bail!("unsupported Container kind in catalog: {kind}"),
    }
}

/// Validates one supported Container kind before persisting a descriptor.
///
/// # Arguments
///
/// * `kind` - Candidate concrete Container kind.
///
/// # Returns
///
/// `Ok(())` for `local`, `link`, or `configurable`.
///
/// # Errors
///
/// Returns an error for any unsupported kind.
fn validate_container_kind(kind: &str) -> Result<()> {
    if matches!(kind, "local" | "link" | "configurable") {
        Ok(())
    } else {
        bail!("unsupported Container kind in catalog: {kind}")
    }
}

/// Adds every proper parent of a logical path to the Subcontainer set.
///
/// # Arguments
///
/// * `subcontainers` - Mutable explicit Subcontainer path set.
/// * `path` - Container or Subcontainer path whose ancestors are inserted.
fn insert_parent_subcontainers(subcontainers: &mut BTreeSet<String>, path: &str) {
    let mut offset = 0;
    while let Some(relative) = path[offset..].find('/') {
        offset += relative;
        subcontainers.insert(path[..offset].to_owned());
        offset += 1;
    }
}

/// Returns a logical path's parent, using an empty string for the root.
///
/// # Arguments
///
/// * `path` - Valid slash-separated logical path.
///
/// # Returns
///
/// The substring before the final slash, or `""` for a root child.
fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Returns a logical path's final member name.
///
/// # Arguments
///
/// * `path` - Valid slash-separated logical path.
///
/// # Returns
///
/// The substring after the final slash, or all of `path` for a root child.
fn final_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Logs catalog paths which are declared as both Container and Subcontainer.
///
/// # Arguments
///
/// * `containers` - All logical Container paths.
/// * `subcontainers` - All logical Subcontainer paths.
fn warn_on_same_name(containers: &BTreeSet<String>, subcontainers: &BTreeSet<String>) {
    for path in containers.intersection(subcontainers) {
        warn!(logical_path = %path, "catalog path is both a Container and Subcontainer; trailing slash selects the Subcontainer");
    }
}
