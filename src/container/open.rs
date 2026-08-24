//! Container metadata loading and concrete-container construction.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{
    CONTAINER_FORMAT_VERSION, CONTAINER_METADATA_FILE, CONTROL_DIR, Container, ContainerMetadata,
    LinkContainer, LocalContainer,
};

impl ContainerMetadata {
    /// Parses metadata from the JSON bytes of a `container.json` file.
    pub fn from_json(data: impl AsRef<[u8]>) -> Result<Self> {
        let metadata: Self =
            serde_json::from_slice(data.as_ref()).context("failed to parse container.json data")?;
        metadata.validate(Path::new("container.json"))?;
        Ok(metadata)
    }

    /// Parses metadata directly from a `container.json` file.
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
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(metadata_path(path.as_ref()))
    }

    /// Opens a concrete container selected by this metadata's `kind`.
    pub fn open(self, path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
        let path = path.into();
        self.validate(&metadata_path(&path))?;
        match self.kind.as_str() {
            "local" => Ok(Box::new(LocalContainer::from_metadata(path, self)?)),
            "link" => Ok(Box::new(LinkContainer::from_metadata(path, self)?)),
            kind => Err(anyhow!("unsupported container kind: {kind}")),
        }
    }

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
pub fn open_container(path: impl Into<PathBuf>) -> Result<Box<dyn Container>> {
    let path = path.into();
    ContainerMetadata::load(&path)?.open(path)
}

/// Parses `container.json` bytes and opens the matching concrete container.
pub fn open_container_from_json(
    path: impl Into<PathBuf>,
    data: impl AsRef<[u8]>,
) -> Result<Box<dyn Container>> {
    ContainerMetadata::from_json(data)?.open(path)
}

pub(crate) fn metadata_path(path: &Path) -> PathBuf {
    path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE)
}
