//! Destination abstractions for mapping logical names to container roots.
//!
//! A destination owns a filesystem namespace and exposes containers through
//! stable string indexes. Concrete destinations decide how an index maps to a
//! path; [`mc`] implements the standard Minecraft directory layout.

use anyhow::Result;

use crate::container::Container;

pub mod mc;
pub use mc::McDestination;

/// Maps logical container indexes to concrete container handles.
pub trait Destination {
    /// Lists the container indexes currently discoverable in this destination.
    ///
    /// # Returns
    ///
    /// A deterministic, lexically sorted list of logical container indexes. The
    /// indexes are suitable for passing to [`Self::open`].
    ///
    /// # Errors
    ///
    /// Returns an error when the destination root or one of its required
    /// directories cannot be inspected.
    fn list(&self) -> Result<Vec<String>>;

    /// Opens or initializes the container identified by a logical index.
    ///
    /// # Arguments
    ///
    /// * `name` - Destination-specific logical index returned by [`Self::list`].
    ///
    /// # Returns
    ///
    /// A boxed container handle rooted at the path represented by `name`.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is malformed or addresses a path outside
    /// the destination namespace, or when the concrete container cannot be
    /// opened or initialized.
    fn open(&self, name: &str) -> Result<Box<dyn Container>>;
}

#[cfg(test)]
mod tests;
