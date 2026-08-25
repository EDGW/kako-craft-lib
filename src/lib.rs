//! Filesystem-backed containers, entries, and validated reciprocal links.
//!
//! `kako-craft-lib` provides local containers for ordinary entries and link
//! containers that combine ordinary entries with validated outgoing symbolic
//! links. Mutations are exposed through write guards so metadata, filesystem
//! state, and reciprocal link records are updated while the container lock is
//! held.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![cfg_attr(not(test), deny(clippy::missing_docs_in_private_items))]

pub mod container;
pub mod destination;
pub mod locator;

mod logging;

#[cfg(test)]
pub(crate) mod tests;
