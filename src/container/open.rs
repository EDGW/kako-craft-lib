//! Container metadata loading and concrete-container construction.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{
    CONTAINER_FORMAT_VERSION, CONTAINER_METADATA_FILE, CONTROL_DIR, ConfigurableContainer,
    Container, ContainerMetadata, LinkContainer, LocalContainer,
};

impl ContainerMetadata {
    /// Parses metadata from the JSON bytes of a `container.json` file.
    ///
    /// # Arguments
    ///
    /// * `data` - UTF-8 JSON bytes containing common container metadata.
    ///
    /// # Returns
    ///
    /// Validated common metadata without opening a container directory.
    ///
    /// # Errors
    ///
    /// Returns an error when JSON is malformed, the format version is
    /// unsupported, or the UID is empty.
    pub fn from_json(data: impl AsRef<[u8]>) -> Result<Self> {
        let metadata: Self =
            serde_json::from_slice(data.as_ref()).context("failed to parse container.json data")?;
        metadata.validate(Path::new("container.json"))?;
        Ok(metadata)
    }

    /// Parses metadata directly from a `container.json` file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the `container.json` document, not its parent root.
    ///
    /// # Returns
    ///
    /// Validated common metadata read from the file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or parsed, the format
    /// version is unsupported, or the UID is empty.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let metadata: Self = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("failed to parse {}", path.display()))?;
        metadata.validate(path)?;
        Ok(metadata)
    }

    /// Loads metadata from a container directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Container root containing `.kcl/container.json`.
    ///
    /// # Returns
    ///
    /// Validated common metadata for that root.
    ///
    /// # Errors
    ///
    /// Returns the same file, parsing, and validation errors as
    /// [`Self::from_file`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(metadata_path(path.as_ref()))
    }

    /// Opens a concrete container selected by this metadata's `kind`.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem root represented by this metadata.
    ///
    /// # Returns
    ///
    /// A boxed [`LocalContainer`], [`LinkContainer`], or
    /// [`ConfigurableContainer`] exposed as
    /// [`dyn Container`](Container).
    ///
    /// # Errors
    ///
    /// Returns an error if metadata is invalid, `kind` is unsupported, the
    /// root does not match the requested implementation, or implementation
    /// setup fails.
    pub fn open(self, path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
        let path = path.into();
        self.validate(&metadata_path(&path))?;
        match self.kind.as_str() {
            "local" => Ok(Box::new(LocalContainer::from_metadata(path, self)?)),
            "link" => Ok(Box::new(LinkContainer::from_metadata(path, self)?)),
            "configurable" => Ok(Box::new(ConfigurableContainer::from_metadata(path, self)?)),
            kind => Err(anyhow!("unsupported container kind: {kind}")),
        }
    }

    /// Validates common metadata independently of a concrete container implementation.
    ///
    /// # Arguments
    ///
    /// * `path` - Metadata-file path included in any validation diagnostic.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the format version is supported and the UID is non-empty.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported version or an empty UID.
    pub(crate) fn validate(&self, path: &Path) -> Result<()> {
        if self.version != CONTAINER_FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported format version {} in {}",
                self.version,
                path.display()
            ));
        }
        if self.uid.is_empty() {
            return Err(anyhow!("container UID is empty in {}", path.display()));
        }
        Ok(())
    }
}

/// Reads `.kcl/container.json` and opens the matching concrete container.
///
/// # Arguments
///
/// * `path` - Container root to inspect and open.
///
/// # Returns
///
/// A boxed concrete container selected from its persisted `kind`.
///
/// # Errors
///
/// Returns an error if common metadata cannot be loaded or the corresponding
/// concrete implementation cannot be opened.
pub fn open_container(path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
    let path = path.into();
    ContainerMetadata::load(&path)?.open(path)
}

/// Parses `container.json` bytes and opens the matching concrete container.
///
/// # Arguments
///
/// * `path` - Container root represented by the supplied metadata.
/// * `data` - UTF-8 JSON bytes containing common container metadata.
///
/// # Returns
///
/// A boxed concrete container selected from the parsed `kind`.
///
/// # Errors
///
/// Returns an error if `data` is invalid or the concrete container cannot be
/// opened at `path`.
pub fn open_container_from_json(
    path: impl Into<PathBuf>,
    data: impl AsRef<[u8]>,
) -> Result<Box<dyn Container>> {
    ContainerMetadata::from_json(data)?.open(path)
}

/// Resolves the common metadata-file path below a container root.
///
/// # Arguments
///
/// * `path` - Container root under which `.kcl/container.json` is stored.
///
/// # Returns
///
/// `path/.kcl/container.json` without accessing the filesystem.
pub(crate) fn metadata_path(path: &Path) -> PathBuf {
    path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE)
}
