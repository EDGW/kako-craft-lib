//! Strongly typed, reversibly serialized Destination, Container, and entry locators.

mod error;
mod model;
mod parse;
mod resolve;

pub use error::LocatorParseError;
pub use model::{ContainerLocator, ContainerPath, DestinationLocator, EntryLocator};
pub use resolve::{initialize_container, resolve_container, resolve_subcontainer};

#[cfg(test)]
mod tests;
