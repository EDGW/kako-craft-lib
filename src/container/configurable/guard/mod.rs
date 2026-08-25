//! Configurable writer state and stable-order transaction lifecycle.

mod automatic;
mod explicit;
mod trait_impl;

use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::container::{ContainerWriteGuard, LinkContainerWriteGuard};
use crate::logging::ContainerLogger;

use super::transaction::{MultiContainerWriteTransaction, TargetParticipant};

/// Exclusive configurable mutation guard.
///
/// The guard normally owns the source lock. Multi-Container mutations release
/// it, acquire every participant in stable path order, then restore the source
/// guard after target locks are dropped.
pub struct ConfigurableContainerWriteGuard {
    /// ConfigurableContainer filesystem root.
    root: PathBuf,
    /// Source writer, absent after an acquisition failure releases all locks.
    source: Option<LinkContainerWriteGuard>,
    /// Stable source UID retained after failed acquisition releases the writer.
    uid: String,
    /// Stable configurable tracing identity.
    logger: ContainerLogger,
}

impl ConfigurableContainerWriteGuard {
    /// Constructs a guard around an already locked configurable source.
    ///
    /// # Arguments
    ///
    /// * `root` - ConfigurableContainer filesystem root.
    /// * `source` - Underlying link-capable source writer.
    /// * `logger` - Configurable tracing identity.
    ///
    /// # Returns
    ///
    /// A ready configurable writer.
    pub(super) fn new(
        root: PathBuf,
        source: LinkContainerWriteGuard,
        logger: ContainerLogger,
    ) -> Self {
        let uid = source.container_uid().to_owned();
        Self {
            root,
            source: Some(source),
            uid,
            logger,
        }
    }

    /// Returns a mutable source guard or a released-writer error.
    ///
    /// # Returns
    ///
    /// The underlying link-capable writer.
    ///
    /// # Errors
    ///
    /// Returns an error after failed multi-lock acquisition released all locks.
    fn source_mut(&mut self) -> Result<&mut LinkContainerWriteGuard> {
        self.source
            .as_mut()
            .ok_or_else(|| anyhow!("configurable writer released after lock acquisition failure"))
    }

    /// Returns an immutable source guard or a released-writer error.
    ///
    /// # Returns
    ///
    /// The underlying link-capable writer.
    ///
    /// # Errors
    ///
    /// Returns an error after failed multi-lock acquisition released all locks.
    fn source(&self) -> Result<&LinkContainerWriteGuard> {
        self.source
            .as_ref()
            .ok_or_else(|| anyhow!("configurable writer released after lock acquisition failure"))
    }

    /// Acquires a stable-order transaction after releasing the initial source lock.
    ///
    /// # Arguments
    ///
    /// * `targets` - UID-verified target participants.
    ///
    /// # Returns
    ///
    /// A transaction owning source and target writers.
    ///
    /// # Errors
    ///
    /// Returns an acquisition error after all partially acquired locks are released.
    fn transaction(
        &mut self,
        targets: Vec<TargetParticipant>,
    ) -> Result<MultiContainerWriteTransaction> {
        self.source.take();
        MultiContainerWriteTransaction::acquire(&self.root, targets)
    }

    /// Restores a transaction's source writer and drops all target guards.
    ///
    /// # Arguments
    ///
    /// * `transaction` - Completed or failed mutation transaction.
    fn restore_source(&mut self, mut transaction: MultiContainerWriteTransaction) {
        self.source = Some(transaction.take_source());
    }
}
