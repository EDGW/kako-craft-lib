//! Minecraft installation Destination and Version catalog.
//!
//! This module models only the managed portions of a Minecraft directory. It
//! deliberately does not scan or register arbitrary user directories such as
//! saves, logs, resourcepacks, or shaderpacks.

mod catalog;
mod destination;
mod version;

#[cfg(test)]
mod tests;

pub use destination::{McDestination, McDestinationProvider};
pub use version::Version;
