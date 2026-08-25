//! Construction and verification of deliberately broken linked-container scenarios.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{BrokenContainersFixture, BrokenFixtureCase, BrokenFixtureErrorKind};
use crate::container::{
    Container, EntryKey, LinkCheckKind, LinkContainer, LinkValidationIssueKind, LocalContainer,
    open_container,
};

/// Ordinary target key used consistently across generated linked pairs.
pub(super) const TARGET_KEY: &str = "target.txt";
/// Outgoing linker key used consistently across generated linked pairs.
pub(super) const LINKER_KEY: &str = "link.txt";
/// Filename of incoming reciprocal metadata below a container control directory.
pub(super) const INCOMING_FILE: &str = "links.json";
/// Filename of outgoing metadata below a link-container control directory.
pub(super) const OUTGOING_FILE: &str = "outgoing-links.json";

/// Every persistent validation category supported by the current validation model.
const ALL_VALIDATION_KINDS: [LinkValidationIssueKind; 17] = [
    LinkValidationIssueKind::MissingOutgoingRecord,
    LinkValidationIssueKind::UnexpectedOutgoingRecord,
    LinkValidationIssueKind::MissingIncomingRecord,
    LinkValidationIssueKind::UnexpectedIncomingRecord,
    LinkValidationIssueKind::LinkerUidMismatch,
    LinkValidationIssueKind::TargetUidMismatch,
    LinkValidationIssueKind::LinkerKeyMismatch,
    LinkValidationIssueKind::TargetKeyMismatch,
    LinkValidationIssueKind::TargetEntryMissing,
    LinkValidationIssueKind::ContainerPathMissing,
    LinkValidationIssueKind::ContainerPathMismatch,
    LinkValidationIssueKind::ContainerPathUidMismatch,
    LinkValidationIssueKind::DuplicateIncomingRecord,
    LinkValidationIssueKind::DuplicateOutgoingRecord,
    LinkValidationIssueKind::MetadataInvalid,
    LinkValidationIssueKind::MaterializedSymlinkMissing,
    LinkValidationIssueKind::MaterializedSymlinkMismatch,
];

/// Validation categories expected to be expressible through built-in filesystem metadata.
const EXPECTED_VALIDATION_KINDS: [LinkValidationIssueKind; 16] = [
    LinkValidationIssueKind::MissingOutgoingRecord,
    LinkValidationIssueKind::UnexpectedOutgoingRecord,
    LinkValidationIssueKind::MissingIncomingRecord,
    LinkValidationIssueKind::UnexpectedIncomingRecord,
    LinkValidationIssueKind::LinkerUidMismatch,
    LinkValidationIssueKind::TargetUidMismatch,
    LinkValidationIssueKind::LinkerKeyMismatch,
    LinkValidationIssueKind::TargetKeyMismatch,
    LinkValidationIssueKind::TargetEntryMissing,
    LinkValidationIssueKind::ContainerPathMissing,
    LinkValidationIssueKind::ContainerPathMismatch,
    LinkValidationIssueKind::ContainerPathUidMismatch,
    LinkValidationIssueKind::DuplicateIncomingRecord,
    LinkValidationIssueKind::MetadataInvalid,
    LinkValidationIssueKind::MaterializedSymlinkMissing,
    LinkValidationIssueKind::MaterializedSymlinkMismatch,
];

/// Creates isolated, mutually linked broken-container scenarios below a new root.
///
/// Each case is first created through the ordinary guarded APIs, deliberately
/// corrupted on disk, and then reopened and inspected through
/// [`ContainerWriteGuard::validate_links`](crate::container::ContainerWriteGuard::validate_links)
/// and [`ContainerWriteGuard::check`](crate::container::ContainerWriteGuard::check).
/// Returned error kinds therefore describe
/// actual validator output rather than a predefined expectation list.
///
/// # Arguments
///
/// * `path` - New fixture root to create. The path must not already exist, preventing accidental
///   modification or deletion of caller-owned data.
///
/// # Returns
///
/// The created container paths, every verified scenario, its actually observed errors, and the
/// validation categories that the built-in persistent schema cannot express.
///
/// # Errors
///
/// Returns an error if `path` already exists, a container or relationship cannot be created, an
/// intentional metadata or symlink mutation fails, generated containers cannot be reopened or
/// validated, or an expected reproducible error category is not observed.
pub fn create_broken_containers(path: impl Into<PathBuf>) -> Result<BrokenContainersFixture> {
    let root = path.into();
    if root.exists() {
        bail!(
            "refusing to create broken-container fixtures in existing path {}",
            root.display()
        );
    }
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create fixture root {}", root.display()))?;

    let mut builder = FixtureBuilder::new(root.clone());
    builder.missing_outgoing_and_unexpected_incoming()?;
    builder.missing_incoming_and_unexpected_outgoing()?;
    builder.linker_uid_mismatch()?;
    builder.target_uid_and_path_uid_mismatch()?;
    builder.linker_key_mismatch()?;
    builder.target_key_and_missing_entry()?;
    builder.target_entry_missing()?;
    builder.container_path_missing()?;
    builder.container_path_mismatch()?;
    builder.duplicate_incoming()?;
    builder.metadata_invalid()?;
    builder.missing_symlink()?;
    builder.incorrect_symlink()?;
    builder.unrecorded_symlink()?;

    builder.finish()
}

/// Paths for one initially valid local-target/linker pair.
pub(super) struct Pair {
    /// Ordinary target container root.
    pub(super) target: PathBuf,
    /// Outgoing link container root.
    pub(super) linker: PathBuf,
}

/// A validation/check invocation to execute after all on-disk mutations are complete.
struct CaseSpec {
    /// Stable scenario name returned to callers.
    name: String,
    /// Current container root.
    current: PathBuf,
    /// Current entry key supplied to validation.
    key: EntryKey,
    /// Corresponding container roots supplied to validation and checking.
    corresponding: Vec<PathBuf>,
    /// Error kinds that must appear if the fixture remains compatible with the validator.
    expected: Vec<BrokenFixtureErrorKind>,
}

/// Stateful fixture constructor collecting paths and deferred verification cases.
pub(super) struct FixtureBuilder {
    /// Root directory under which every scenario is isolated.
    pub(super) root: PathBuf,
    /// Every created concrete container root.
    pub(super) containers: Vec<PathBuf>,
    /// Deferred cases inspected after their corruption is complete.
    specs: Vec<CaseSpec>,
}

impl FixtureBuilder {
    /// Starts a fixture build below an already created root.
    ///
    /// # Arguments
    ///
    /// * `root` - Newly created, caller-owned fixture root.
    ///
    /// # Returns
    ///
    /// An empty builder ready to create scenarios.
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            containers: Vec::new(),
            specs: Vec::new(),
        }
    }

    /// Creates a valid absolute-path outgoing link for one isolated scenario.
    ///
    /// # Arguments
    ///
    /// * `scenario` - Directory name created immediately below the fixture root.
    ///
    /// # Returns
    ///
    /// Target and linker roots after `link.txt` validly targets `target.txt`.
    ///
    /// # Errors
    ///
    /// Returns an error when either container, the target entry, or the reciprocal relationship
    /// cannot be created.
    pub(super) fn pair(&mut self, scenario: &str) -> Result<Pair> {
        let scenario_root = self.root.join(scenario);
        let target_path = scenario_root.join("target");
        let linker_path = scenario_root.join("linker");
        let mut target =
            LocalContainer::with_logical_name(&target_path, format!("{scenario}-target"))?;
        let linker = LinkContainer::with_logical_name(&linker_path, format!("{scenario}-linker"))?;
        target.writer()?.write(
            &TARGET_KEY.to_owned(),
            format!("fixture:{scenario}\n").into_bytes(),
        )?;
        linker.writer()?.link_to(
            &LINKER_KEY.to_owned(),
            &mut target,
            &TARGET_KEY.to_owned(),
            Some(false),
        )?;
        self.containers.push(target_path.clone());
        self.containers.push(linker_path.clone());
        Ok(Pair {
            target: target_path,
            linker: linker_path,
        })
    }

    /// Records a deferred validation/check invocation.
    ///
    /// # Arguments
    ///
    /// * `name` - Stable descriptive case name.
    /// * `current` - Current container root.
    /// * `key` - Current entry key to validate.
    /// * `corresponding` - Peer roots supplied to validation and checking.
    /// * `expected` - Error kinds required from actual inspection.
    ///
    /// # Returns
    ///
    /// Returns after appending the case specification.
    pub(super) fn case(
        &mut self,
        name: &str,
        current: &Path,
        key: &str,
        corresponding: &[&Path],
        expected: Vec<BrokenFixtureErrorKind>,
    ) {
        self.specs.push(CaseSpec {
            name: name.to_owned(),
            current: current.to_owned(),
            key: key.to_owned(),
            corresponding: corresponding
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            expected,
        });
    }

    /// Reopens and verifies every deferred case, then checks aggregate coverage.
    ///
    /// # Returns
    ///
    /// The completed public fixture report containing actual observed errors.
    ///
    /// # Errors
    ///
    /// Returns an error when a case cannot be inspected, an expected error is absent, or any
    /// reproducible validation category is missing from aggregate coverage.
    fn finish(mut self) -> Result<BrokenContainersFixture> {
        let mut cases = Vec::new();
        for spec in self.specs {
            let case = inspect_case(spec)?;
            cases.push(case);
        }
        let observed_validation = cases
            .iter()
            .flat_map(|case| case.errors.iter())
            .filter_map(|kind| match kind {
                BrokenFixtureErrorKind::Validation(kind) => Some(*kind),
                _ => None,
            })
            .collect::<Vec<_>>();
        for expected in EXPECTED_VALIDATION_KINDS {
            if !observed_validation.contains(&expected) {
                bail!("broken fixture did not reproduce expected validation kind {expected:?}");
            }
        }
        self.containers.sort();
        self.containers.dedup();
        let uncovered_validation_kinds = ALL_VALIDATION_KINDS
            .into_iter()
            .filter(|kind| !observed_validation.contains(kind))
            .collect();
        Ok(BrokenContainersFixture {
            root: self.root,
            containers: self.containers,
            cases,
            uncovered_validation_kinds,
        })
    }
}

/// Inspects one generated case through the normal validation and check APIs.
///
/// # Arguments
///
/// * `spec` - Current root, key, peers, and required actual errors for the case.
///
/// # Returns
///
/// A public case containing deduplicated actual validation and filesystem check errors.
///
/// # Errors
///
/// Returns an error if containers cannot be opened or locked, inspection fails, or a required error
/// kind is absent.
fn inspect_case(spec: CaseSpec) -> Result<BrokenFixtureCase> {
    let current = open_container(&spec.current)
        .with_context(|| format!("failed to open fixture current container {}", spec.name))?;
    let corresponding = spec
        .corresponding
        .iter()
        .map(open_container)
        .collect::<Result<Vec<_>>>()?;
    let refs = corresponding
        .iter()
        .map(|container| container.as_ref())
        .collect::<Vec<_>>();
    let mut writer = current.writer()?;
    let validation = writer.validate_links(&spec.key, &refs)?;
    let mut errors = validation
        .broken
        .into_iter()
        .map(|issue| BrokenFixtureErrorKind::Validation(issue.kind))
        .collect::<Vec<_>>();
    if !validation.unavailable.is_empty() {
        push_unique(&mut errors, BrokenFixtureErrorKind::Unavailable);
    }
    for issue in writer.check(&refs)? {
        let kind = match issue.kind {
            LinkCheckKind::MissingSymlink => BrokenFixtureErrorKind::MissingSymlink,
            LinkCheckKind::IncorrectSymlink => BrokenFixtureErrorKind::IncorrectSymlink,
            LinkCheckKind::UnrecordedSymlink => BrokenFixtureErrorKind::UnrecordedSymlink,
            LinkCheckKind::Validation(kind) => BrokenFixtureErrorKind::Validation(kind),
            LinkCheckKind::Unavailable => BrokenFixtureErrorKind::Unavailable,
        };
        push_unique(&mut errors, kind);
    }
    for expected in &spec.expected {
        if !errors.contains(expected) {
            bail!(
                "broken fixture case '{}' expected {expected:?}, observed {errors:?}",
                spec.name
            );
        }
    }
    Ok(BrokenFixtureCase {
        name: spec.name,
        current_container: spec.current,
        key: spec.key,
        corresponding_containers: spec.corresponding,
        errors,
    })
}

/// Wraps a validation kind as a fixture error kind.
///
/// # Arguments
///
/// * `kind` - Persistent validation category to wrap.
///
/// # Returns
///
/// The corresponding [`BrokenFixtureErrorKind::Validation`] value.
pub(super) fn validation(kind: LinkValidationIssueKind) -> BrokenFixtureErrorKind {
    BrokenFixtureErrorKind::Validation(kind)
}

/// Appends an error kind only if the same kind has not already been observed.
///
/// # Arguments
///
/// * `errors` - Ordered case error list to update.
/// * `kind` - Newly observed kind.
///
/// # Returns
///
/// Returns after preserving the first-observation order and suppressing duplicates.
fn push_unique(errors: &mut Vec<BrokenFixtureErrorKind>, kind: BrokenFixtureErrorKind) {
    if !errors.contains(&kind) {
        errors.push(kind);
    }
}
