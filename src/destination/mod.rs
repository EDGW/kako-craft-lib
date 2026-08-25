//! Explicit Destination catalogs and logical Container/Subcontainer traversal.
//!
//! A Destination is a filesystem-rooted catalog, not a Container. Its catalog
//! exposes only declared members; unrelated directories are never discovered
//! as managed Containers.

mod descriptor;
mod fake;
pub mod minecraft;
mod provider;
mod traits;

pub mod catalog;

pub use catalog::{CatalogDestination, CatalogDestinationProvider};
pub use descriptor::{ContainerDescriptor, DestinationMember, SubcontainerDescriptor};
pub use fake::FakeDestination;
pub use provider::{
    DestinationMetadata, DestinationProvider, DestinationRegistry, open_destination,
};
pub use traits::{Destination, Subcontainer};

#[cfg(test)]
mod tests;
