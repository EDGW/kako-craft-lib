//! Minecraft-directory destination and its logical container indexes.
//!
//! A Minecraft root-level directory is addressed by its directory name, for
//! example `libraries` or `assets`. The currently supported version-specific
//! container is addressed as `versions:<version>:mods` and maps to
//! `<minecraft-root>/versions/<version>/mods`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::container::{CONTROL_DIR, Container, LocalContainer};
use crate::destination::Destination;

/// Logical namespace separator used by version-specific Minecraft indexes.
const INDEX_SEPARATOR: char = ':';
/// Prefix used by version-specific indexes.
const VERSIONS_PREFIX: &str = "versions:";
/// Suffix identifying the only currently supported version container.
const MODS_SUFFIX: &str = ":mods";

/// A destination rooted at one standard Minecraft installation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McDestination {
    /// Filesystem root of the Minecraft installation, commonly `~/.minecraft`.
    path: PathBuf,
}

impl McDestination {
    /// Opens a Minecraft destination rooted at an existing directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Existing Minecraft installation root, such as
    ///   `/home/user/.minecraft` or `~/.minecraft` after caller-side tilde
    ///   expansion.
    ///
    /// # Returns
    ///
    /// A destination handle retaining the supplied path without canonicalizing
    /// it or creating any child containers.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` does not exist, is not a directory, or its
    /// metadata cannot be inspected.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to inspect Minecraft root {}", path.display()))?;
        if !metadata.is_dir() {
            bail!("Minecraft root is not a directory: {}", path.display());
        }
        Ok(Self { path })
    }

    /// Returns the filesystem root of this Minecraft destination.
    ///
    /// # Returns
    ///
    /// A borrowed path valid for the lifetime of this destination handle.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves a logical index to its filesystem container root without opening it.
    ///
    /// # Arguments
    ///
    /// * `name` - A root-level directory name such as `libraries`, or a
    ///   version-specific index in the exact form `versions:<version>:mods`.
    ///
    /// # Returns
    ///
    /// The path represented by `name`. The path may not exist yet; callers can
    /// pass it to [`Self::open`] to initialize a local container there.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is empty, absolute, contains path traversal,
    /// contains a platform separator or reserved control-directory component,
    /// has an unsupported version-index shape, or names `versions` directly.
    pub fn container_path(&self, name: &str) -> Result<PathBuf> {
        let relative = parse_index(name)?;
        Ok(self.path.join(relative))
    }

    /// Opens or initializes a local container for a Minecraft logical index.
    ///
    /// # Arguments
    ///
    /// * `name` - Root-level or `versions:<version>:mods` index accepted by
    ///   [`Self::container_path`].
    ///
    /// # Returns
    ///
    /// A [`LocalContainer`] erased behind the [`Container`] trait. Missing
    /// standard directories are created with container metadata on first open.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is invalid, the resolved path is occupied
    /// by a non-directory, or local-container initialization fails.
    pub fn open_local(&self, name: &str) -> Result<LocalContainer> {
        let path = self.container_path(name)?;
        LocalContainer::new(path)
    }
}

impl Destination for McDestination {
    /// Lists root-level Minecraft directories and existing version `mods`
    /// directories as logical indexes.
    ///
    /// Root-level `.kcl` and `versions` directories are excluded from the
    /// root-level results. A version is listed only when its `mods` directory
    /// exists; opening an absent version `mods` index can create it explicitly.
    ///
    /// # Returns
    ///
    /// Sorted, deduplicated indexes such as `assets`, `libraries`, and
    /// `versions:1.19.2:mods`.
    ///
    /// # Errors
    ///
    /// Returns an error when the Minecraft root, its directory entries, the
    /// `versions` directory, or a version entry cannot be inspected.
    fn list(&self) -> Result<Vec<String>> {
        let mut indexes = Vec::new();
        let entries = fs::read_dir(&self.path)
            .with_context(|| format!("failed to list Minecraft root {}", self.path.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == CONTROL_DIR || name == "versions" {
                continue;
            }
            if entry.file_type()?.is_dir() && is_safe_component(name) {
                indexes.push(name.to_owned());
            }
        }

        let versions = self.path.join("versions");
        if versions.is_dir() {
            for entry in fs::read_dir(&versions).with_context(|| {
                format!(
                    "failed to list Minecraft versions directory {}",
                    versions.display()
                )
            })? {
                let entry = entry?;
                let version = entry.file_name();
                let Some(version) = version.to_str() else {
                    continue;
                };
                if is_safe_component(version) && entry.file_type()?.is_dir() {
                    let mods = entry.path().join("mods");
                    if mods.is_dir() {
                        indexes.push(format!("versions:{version}:mods"));
                    }
                }
            }
        }
        indexes.sort();
        indexes.dedup();
        Ok(indexes)
    }

    /// Opens or initializes the local container represented by a Minecraft index.
    ///
    /// # Arguments
    ///
    /// * `name` - Root-level or version-specific logical index.
    ///
    /// # Returns
    ///
    /// A boxed [`LocalContainer`] implementing the common [`Container`] API.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is malformed, resolves outside the
    /// Minecraft root, or local-container initialization fails.
    fn open(&self, name: &str) -> Result<Box<dyn Container>> {
        Ok(Box::new(self.open_local(name)?))
    }
}

/// Parses one Minecraft destination index into a root-relative path.
///
/// # Arguments
///
/// * `name` - Logical index supplied to [`McDestination::container_path`].
///
/// # Returns
///
/// A safe relative path: one component for root containers or
/// `versions/<version>/mods` for version-specific containers.
///
/// # Errors
///
/// Returns an error for unsupported syntax, empty components, path separators,
/// traversal, or the reserved `versions` root index.
fn parse_index(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        bail!("invalid Minecraft container index: {name:?}");
    }
    if let Some(version) = name
        .strip_prefix(VERSIONS_PREFIX)
        .and_then(|value| value.strip_suffix(MODS_SUFFIX))
    {
        if !is_safe_component(version) || version.contains(INDEX_SEPARATOR) {
            bail!("invalid Minecraft version container index: {name:?}");
        }
        return Ok(PathBuf::from("versions").join(version).join("mods"));
    }
    if name == "versions" || name.contains(INDEX_SEPARATOR) || !is_safe_component(name) {
        bail!("unsupported Minecraft container index: {name:?}");
    }
    Ok(PathBuf::from(name))
}

/// Reports whether a string is one safe normal path component.
///
/// # Arguments
///
/// * `value` - Candidate directory or version component.
///
/// # Returns
///
/// `true` when the value is non-empty, contains no path separator or colon,
/// is not `.` or `..`, and consists of a normal filesystem component.
fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value != CONTROL_DIR
        && !value.contains(INDEX_SEPARATOR)
        && !value.contains('/')
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .eq([Component::Normal(std::ffi::OsStr::new(value))])
}
