//! Object-safe Destination and Subcontainer interfaces.

use std::path::Path;

use anyhow::Result;

use crate::container::Container;

use super::{ContainerDescriptor, SubcontainerDescriptor};

/// One node in a Destination's logical member tree.
pub trait Subcontainer: Send + Sync {
    /// Clones this lightweight catalog node behind an object-safe pointer.
    ///
    /// # Returns
    ///
    /// A boxed handle representing the same logical node.
    fn boxed_clone(&self) -> Box<dyn Subcontainer>;

    /// Lists explicitly declared direct Container children.
    ///
    /// # Returns
    ///
    /// Stable lexically ordered descriptors without opening the Containers.
    ///
    /// # Errors
    ///
    /// Returns an error when authoritative catalog metadata cannot be read.
    fn containers(&self) -> Result<Vec<ContainerDescriptor>>;

    /// Lists explicitly declared direct Subcontainer children.
    ///
    /// # Returns
    ///
    /// Stable lexically ordered descriptors without recursively opening them.
    ///
    /// # Errors
    ///
    /// Returns an error when authoritative catalog metadata cannot be read.
    fn subcontainers(&self) -> Result<Vec<SubcontainerDescriptor>>;

    /// Opens or initializes a declared direct Container child.
    ///
    /// # Arguments
    ///
    /// * `name` - Direct child name, not a slash-delimited path.
    ///
    /// # Returns
    ///
    /// A boxed concrete Container selected by the catalog descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or undeclared names, filesystem failures,
    /// or a persisted Container kind that contradicts the catalog.
    fn open_container(&self, name: &str) -> Result<Box<dyn Container>>;

    /// Opens a declared direct Subcontainer child.
    ///
    /// # Arguments
    ///
    /// * `name` - Direct child name, not a slash-delimited path.
    ///
    /// # Returns
    ///
    /// A boxed logical catalog node.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or undeclared names or unreadable catalog metadata.
    fn open_subcontainer(&self, name: &str) -> Result<Box<dyn Subcontainer>>;
}

/// Root node of one filesystem-backed logical catalog.
pub trait Destination: Subcontainer {
    /// Returns the Destination's filesystem root.
    ///
    /// # Returns
    ///
    /// A borrowed path valid for this handle's lifetime.
    fn root_path(&self) -> &Path;

    /// Returns the provider kind persisted in Destination metadata.
    ///
    /// # Returns
    ///
    /// A stable provider identifier such as `catalog`.
    fn kind(&self) -> &str;
}
