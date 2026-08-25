//! Deliberately inconsistent filesystem fixtures for exercising container diagnostics.
//!
//! These APIs create new test-only container trees and then inspect the generated
//! state through the normal validation and check interfaces. They never modify an
//! existing path.

mod broken;
mod filesystem;
mod scenarios;

pub use broken::create_broken_containers;

use std::path::PathBuf;

use super::{EntryKey, LinkValidationIssueKind};

/// One error category actually observed after generating a broken fixture case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokenFixtureErrorKind {
    /// A persistent reciprocal-record, identity, path, target, or symlink validation failure.
    Validation(LinkValidationIssueKind),
    /// An outgoing metadata record has no materialized symbolic link.
    MissingSymlink,
    /// An outgoing metadata record has a materialized entry with the wrong symlink target or type.
    IncorrectSymlink,
    /// A symbolic link exists in a link container without an outgoing metadata record.
    UnrecordedSymlink,
    /// A corresponding container unexpectedly could not be inspected while verifying the fixture.
    Unavailable,
}

/// A generated scenario and the errors observed by normal library validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenFixtureCase {
    /// Stable directory and scenario name describing the intentional corruption.
    pub name: String,
    /// Container root used as the current side of validation and checking.
    pub current_container: PathBuf,
    /// Current-container entry key supplied to reciprocal validation.
    pub key: EntryKey,
    /// Corresponding container roots supplied to reciprocal validation and checking.
    pub corresponding_containers: Vec<PathBuf>,
    /// Deduplicated error kinds actually returned after the corruption was written.
    pub errors: Vec<BrokenFixtureErrorKind>,
}

/// Complete result of generating and verifying a broken-container tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenContainersFixture {
    /// Root directory created by the generator.
    pub root: PathBuf,
    /// Every local or link container root created below [`Self::root`].
    pub containers: Vec<PathBuf>,
    /// Validation/check invocations and their actually observed error kinds.
    pub cases: Vec<BrokenFixtureCase>,
    /// Validation categories not expressible by the generated built-in filesystem containers.
    pub uncovered_validation_kinds: Vec<LinkValidationIssueKind>,
}
