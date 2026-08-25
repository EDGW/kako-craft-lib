//! Non-enumerable filesystem-relative Destination adapter.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::container::{Container, open_container};

use super::{ContainerDescriptor, Subcontainer, SubcontainerDescriptor};

/// A lookup-only adapter rooted at an arbitrary working directory.
#[derive(Debug, Clone)]
pub struct FakeDestination {
    /// Filesystem base for relative Container paths.
    root: PathBuf,
}

impl FakeDestination {
    /// Creates a lookup-only adapter.
    ///
    /// # Arguments
    ///
    /// * `root` - Base directory for relative Container paths.
    ///
    /// # Returns
    ///
    /// A FakeDestination which never scans or enumerates `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the adapter's filesystem base.
    ///
    /// # Returns
    ///
    /// A borrowed path valid for the adapter's lifetime.
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Resolves a raw Container path below the fake root.
    ///
    /// # Arguments
    ///
    /// * `value` - Relative or absolute Container filesystem path.
    ///
    /// # Returns
    ///
    /// An absolute input unchanged, otherwise `root.join(value)`.
    fn resolve(&self, value: &str) -> PathBuf {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        }
    }
}

impl Subcontainer for FakeDestination {
    fn boxed_clone(&self) -> Box<dyn Subcontainer> {
        Box::new(self.clone())
    }

    fn containers(&self) -> Result<Vec<ContainerDescriptor>> {
        Ok(Vec::new())
    }

    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>> {
        Ok(Vec::new())
    }

    fn open_container(&self, name: &str) -> Result<Box<dyn Container>> {
        open_container(self.resolve(name))
    }

    fn open_subcontainer(&self, name: &str) -> Result<Box<dyn Subcontainer>> {
        let path = self.resolve(name);
        if !path.is_dir() {
            bail!(
                "FakeDestination subcontainer does not exist: {}",
                path.display()
            );
        }
        Ok(Box::new(Self::new(path)))
    }
}
