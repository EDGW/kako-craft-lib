//! Destination-root metadata, providers, and provider registry.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::container::CONTROL_DIR;

use super::Destination;
use super::catalog::CatalogDestinationProvider;

/// Current `.kcl/destination.json` schema version.
pub const DESTINATION_FORMAT_VERSION: u32 = 1;
/// Destination metadata filename below its root control directory.
pub const DESTINATION_METADATA_FILE: &str = "destination.json";

/// Common metadata used to select a Destination provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationMetadata {
    /// On-disk metadata schema version.
    pub version: u32,
    /// Provider identifier used to reopen this Destination.
    pub kind: String,
}

impl DestinationMetadata {
    /// Reads and validates common metadata from a Destination root.
    ///
    /// # Arguments
    ///
    /// * `root` - Filesystem root containing `.kcl/destination.json`.
    ///
    /// # Returns
    ///
    /// Validated common Destination metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or parsed, its version
    /// is unsupported, or its provider kind is empty.
    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let path = metadata_path(root.as_ref());
        let metadata: Self = serde_json::from_reader(BufReader::new(
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?,
        ))
        .with_context(|| format!("failed to parse {}", path.display()))?;
        if metadata.version != DESTINATION_FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported destination format version {} in {}",
                metadata.version,
                path.display()
            ));
        }
        if metadata.kind.is_empty() {
            return Err(anyhow!("destination kind is empty in {}", path.display()));
        }
        Ok(metadata)
    }
}

/// Opens one concrete Destination kind.
pub trait DestinationProvider: Send + Sync {
    /// Returns the stable provider identifier.
    ///
    /// # Returns
    ///
    /// The exact value persisted in [`DestinationMetadata::kind`].
    fn kind(&self) -> &'static str;

    /// Opens an existing Destination root.
    ///
    /// # Arguments
    ///
    /// * `root` - Root whose common metadata already selected this provider.
    ///
    /// # Returns
    ///
    /// A boxed concrete Destination.
    ///
    /// # Errors
    ///
    /// Returns an error when provider-specific metadata is missing or invalid.
    fn open(&self, root: &Path) -> Result<Box<dyn Destination>>;
}

/// Explicit provider set used by locator resolution.
pub struct DestinationRegistry {
    /// Providers searched by exact stable kind.
    providers: Vec<Box<dyn DestinationProvider>>,
}

impl Default for DestinationRegistry {
    fn default() -> Self {
        Self {
            providers: vec![Box::new(CatalogDestinationProvider)],
        }
    }
}

impl DestinationRegistry {
    /// Creates the standard stage-one registry.
    ///
    /// # Returns
    ///
    /// A registry containing the explicit catalog provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one provider, replacing no existing provider implicitly.
    ///
    /// # Arguments
    ///
    /// * `provider` - Provider to append to exact-kind lookup order.
    ///
    /// # Returns
    ///
    /// This registry for builder-style composition.
    pub fn with_provider(mut self, provider: impl DestinationProvider + 'static) -> Self {
        self.providers.push(Box::new(provider));
        self
    }

    /// Opens a Destination after selecting its provider from common metadata.
    ///
    /// # Arguments
    ///
    /// * `root` - Existing Destination root.
    ///
    /// # Returns
    ///
    /// A boxed concrete Destination.
    ///
    /// # Errors
    ///
    /// Returns an error when common metadata is invalid, no provider supports
    /// its kind, or provider-specific opening fails.
    pub fn open(&self, root: impl Into<PathBuf>) -> Result<Box<dyn Destination>> {
        let root = root.into();
        let metadata = DestinationMetadata::load(&root)?;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.kind() == metadata.kind)
            .ok_or_else(|| anyhow!("unsupported destination kind: {}", metadata.kind))?;
        provider.open(&root)
    }
}

/// Opens a Destination with the standard provider registry.
///
/// # Arguments
///
/// * `root` - Existing Destination root containing common metadata.
///
/// # Returns
///
/// A boxed concrete Destination.
///
/// # Errors
///
/// Returns the common metadata, provider selection, or provider opening error.
pub fn open_destination(root: impl Into<PathBuf>) -> Result<Box<dyn Destination>> {
    DestinationRegistry::new().open(root)
}

/// Resolves the common Destination metadata path.
///
/// # Arguments
///
/// * `root` - Destination filesystem root.
///
/// # Returns
///
/// `<root>/.kcl/destination.json` without filesystem access.
pub(crate) fn metadata_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join(DESTINATION_METADATA_FILE)
}
