//! Stable-order multi-Container writer acquisition for configurable mutations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::container::{
    Container, ContainerMetadata, ContainerWriteGuard, LinkContainer, LinkContainerWriteGuard,
};
use crate::locator::{ContainerLocator, resolve_container};

/// An opened target Container awaiting stable-order lock acquisition.
pub(super) struct TargetParticipant {
    /// Absolute-form root used for ordering and lookup.
    pub(super) root: PathBuf,
    /// Verified stable target UID.
    pub(super) uid: String,
    /// Opened target handle used to acquire its writer.
    container: Box<dyn Container>,
}

/// A target whose writer is owned by a multi-lock transaction.
pub(super) struct LockedTarget {
    /// Absolute-form target root.
    pub(super) root: PathBuf,
    /// Stable target UID.
    pub(super) uid: String,
    /// Exclusive target writer.
    pub(super) guard: Box<dyn ContainerWriteGuard>,
}

/// All source and target locks required by one configurable mutation.
pub(super) struct MultiContainerWriteTransaction {
    /// Configurable source writer.
    source: Option<LinkContainerWriteGuard>,
    /// Target writers sorted by root path.
    targets: Vec<LockedTarget>,
}

impl MultiContainerWriteTransaction {
    /// Acquires source and deduplicated target writers in stable root-path order.
    ///
    /// # Arguments
    ///
    /// * `source_root` - ConfigurableContainer root.
    /// * `targets` - Opened, UID-verified target participants.
    ///
    /// # Returns
    ///
    /// A transaction owning every required writer.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate UIDs at different roots, self-targeting,
    /// source reopening failures, or any non-blocking writer acquisition failure.
    /// All writers acquired by this call are dropped before an error returns.
    pub(super) fn acquire(source_root: &Path, mut targets: Vec<TargetParticipant>) -> Result<Self> {
        let source_root = absolute(source_root)?;
        targets.sort_by(|left, right| left.root.cmp(&right.root));
        targets.dedup_by(|left, right| left.root == right.root && left.uid == right.uid);
        let mut uid_roots = BTreeMap::new();
        for target in &targets {
            if let Some(existing) = uid_roots.insert(target.uid.clone(), target.root.clone())
                && existing != target.root
            {
                bail!("target UID {} resolves through multiple roots", target.uid);
            }
        }
        if targets.iter().any(|target| target.root == source_root) {
            bail!("a ConfigurableContainer rule cannot target itself");
        }

        let source_index = targets.partition_point(|target| target.root < source_root);
        let mut source = None;
        let mut locked_targets = Vec::with_capacity(targets.len());
        for (index, target) in targets.into_iter().enumerate() {
            if index == source_index {
                source = Some(acquire_source(&source_root)?);
            }
            let guard = target.container.writer().map_err(|error| {
                anyhow::Error::new(error).context(format!(
                    "target Container {} at {} is unavailable",
                    target.uid,
                    target.root.display()
                ))
            })?;
            locked_targets.push(LockedTarget {
                root: target.root,
                uid: target.uid,
                guard,
            });
        }
        if source.is_none() {
            source = Some(acquire_source(&source_root)?);
        }
        Ok(Self {
            source,
            targets: locked_targets,
        })
    }

    /// Returns the locked configurable source writer.
    ///
    /// # Returns
    ///
    /// A mutable source guard reference.
    pub(super) fn source(&mut self) -> &mut LinkContainerWriteGuard {
        self.source.as_mut().expect("transaction owns source")
    }

    /// Finds a target writer by its verified UID.
    ///
    /// # Arguments
    ///
    /// * `uid` - Stable UID verified before acquisition.
    ///
    /// # Returns
    ///
    /// The matching locked target.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction has no such participant.
    pub(super) fn target(&mut self, uid: &str) -> Result<&mut LockedTarget> {
        self.targets
            .iter_mut()
            .find(|target| target.uid == uid)
            .ok_or_else(|| anyhow!("transaction has no target Container UID {uid}"))
    }

    /// Separates mutable source and target references for one operation.
    ///
    /// # Arguments
    ///
    /// * `uid` - Target UID to select.
    ///
    /// # Returns
    ///
    /// Mutable references to the source guard and selected target.
    ///
    /// # Errors
    ///
    /// Returns an error when no target has `uid`.
    pub(super) fn source_and_target(
        &mut self,
        uid: &str,
    ) -> Result<(&mut LinkContainerWriteGuard, &mut LockedTarget)> {
        let index = self
            .targets
            .iter()
            .position(|target| target.uid == uid)
            .ok_or_else(|| anyhow!("transaction has no target Container UID {uid}"))?;
        Ok((
            self.source.as_mut().expect("transaction owns source"),
            &mut self.targets[index],
        ))
    }

    /// Removes and returns the source guard while dropping targets with this transaction.
    ///
    /// # Returns
    ///
    /// The source guard for restoration into a long-lived configurable writer.
    pub(super) fn take_source(&mut self) -> LinkContainerWriteGuard {
        self.source.take().expect("transaction owns source")
    }
}

/// Resolves a target locator and verifies its persisted UID before locking.
///
/// # Arguments
///
/// * `pwd` - ConfigurableContainer root used as locator working directory.
/// * `locator` - Target Container locator.
/// * `expected_uid` - UID persisted with the rule or outgoing record.
///
/// # Returns
///
/// An opened target participant ready for stable-order acquisition.
///
/// # Errors
///
/// Returns an error when locator resolution or UID reading fails, or the UID mismatches.
pub(super) fn resolve_target(
    pwd: &Path,
    locator: &ContainerLocator,
    expected_uid: &str,
) -> Result<TargetParticipant> {
    let container = resolve_container(locator, pwd)?;
    participant(container, expected_uid)
}

/// Opens the target represented by an outgoing record path.
///
/// # Arguments
///
/// * `source_root` - Root used to resolve a relative outgoing path.
/// * `recorded_path` - Absolute or source-root-relative target path.
/// * `expected_uid` - UID stored in outgoing metadata.
///
/// # Returns
///
/// An opened, UID-verified target participant.
///
/// # Errors
///
/// Returns an error when the target cannot be opened or its UID mismatches.
pub(super) fn resolve_recorded_target(
    source_root: &Path,
    recorded_path: &Path,
    expected_uid: &str,
) -> Result<TargetParticipant> {
    let path = if recorded_path.is_absolute() {
        recorded_path.to_owned()
    } else {
        source_root.join(recorded_path)
    };
    participant(crate::container::open_container(&path)?, expected_uid)
}

/// Reopens a caller-supplied target as a transaction participant.
///
/// # Arguments
///
/// * `container` - Target handle whose root and UID identify the participant.
///
/// # Returns
///
/// A separately opened, UID-verified participant suitable for ordered locking.
///
/// # Errors
///
/// Returns an error when UID/root access, reopening, or identity validation fails.
pub(super) fn reopen_target(container: &dyn Container) -> Result<TargetParticipant> {
    let expected_uid = container.uid()?;
    let root = absolute(&container.root_path())?;
    participant(crate::container::open_container(root)?, &expected_uid)
}

/// Converts an opened target into a verified participant.
///
/// # Arguments
///
/// * `container` - Opened target Container.
/// * `expected_uid` - UID required by metadata.
///
/// # Returns
///
/// A participant with an absolute-form root.
///
/// # Errors
///
/// Returns an error when UID reading/validation or path absolutization fails.
fn participant(container: Box<dyn Container>, expected_uid: &str) -> Result<TargetParticipant> {
    let actual_uid = container.uid()?;
    let root = absolute(&container.root_path())?;
    if actual_uid != expected_uid {
        bail!(
            "target Container UID mismatch at {}: expected {}, found {}",
            root.display(),
            expected_uid,
            actual_uid
        );
    }
    Ok(TargetParticipant {
        root,
        uid: actual_uid,
        container,
    })
}

/// Reopens configurable link-capable storage and acquires its concrete writer.
///
/// # Arguments
///
/// * `root` - Absolute-form ConfigurableContainer root.
///
/// # Returns
///
/// A concrete link-capable writer owning the source lock.
///
/// # Errors
///
/// Returns an error when metadata/opening or non-blocking lock acquisition fails.
fn acquire_source(root: &Path) -> Result<LinkContainerWriteGuard> {
    let metadata = ContainerMetadata::load(root)?;
    let source = LinkContainer::from_metadata_as(root.to_owned(), metadata, "configurable")?;
    source.writer().map_err(Into::into)
}

/// Produces a canonical absolute path for stable lock ordering and deduplication.
///
/// # Arguments
///
/// * `path` - Existing Container root to canonicalize.
///
/// # Returns
///
/// The filesystem's canonical absolute path.
///
/// # Errors
///
/// Returns an error when the root does not exist or cannot be canonicalized.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize Container root {}", path.display()))
}
