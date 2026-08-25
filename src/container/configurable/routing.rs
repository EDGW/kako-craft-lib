//! Ordered rule selection and content-addressed target keys.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::container::{Buffer, EntryKey};
use crate::locator::ContainerLocator;

use super::metadata::ConfigRulesMetadata;

/// Storage class selected explicitly or automatically for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageClass {
    /// Ordinary local filesystem entry.
    Local,
    /// Validated outgoing link to another Container.
    Link,
}

/// Automatic routing result for one entry key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "kebab-case")]
pub enum RouteDecision {
    /// No pattern matched, so ordinary data remains local.
    Local,
    /// The first matching rule routes content by SHA-1 into another Container.
    Link {
        /// Pattern which won first-match evaluation.
        pattern: String,
        /// Target Container locator.
        locator: ContainerLocator,
        /// Stable UID expected from the target.
        container_uid: String,
    },
}

impl RouteDecision {
    /// Returns the storage class represented by this decision.
    ///
    /// # Returns
    ///
    /// [`StorageClass::Local`] or [`StorageClass::Link`].
    pub fn storage_class(&self) -> StorageClass {
        match self {
            Self::Local => StorageClass::Local,
            Self::Link { .. } => StorageClass::Link,
        }
    }
}

/// Structured warning when a forced operation contradicts automatic routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForcedStorageWarning {
    /// Entry key affected by the forced operation.
    pub key: EntryKey,
    /// Storage class selected explicitly by the caller.
    pub forced: StorageClass,
    /// Storage decision a subsequent ordinary write will apply.
    pub automatic: RouteDecision,
}

/// Internal data-bearing route used by mutation transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DataRoute {
    /// Store bytes as an ordinary entry.
    Local,
    /// Store/reuse bytes at a content-addressed target and link to it.
    Link {
        /// Winning pattern used for INFO logging.
        pattern: String,
        /// Target locator.
        locator: ContainerLocator,
        /// Expected target UID.
        container_uid: String,
        /// Two-level SHA-1 target entry key.
        target_key: EntryKey,
    },
}

/// Selects the first matching rule without hashing data.
///
/// # Arguments
///
/// * `root` - ConfigurableContainer root for filesystem case behavior.
/// * `metadata` - Ordered current rules.
/// * `key` - Entry key to match.
///
/// # Returns
///
/// A public local/link decision.
///
/// # Errors
///
/// Returns an error when a persisted pattern cannot be compiled.
pub(super) fn decide(
    root: &Path,
    metadata: &ConfigRulesMetadata,
    key: &EntryKey,
) -> Result<RouteDecision> {
    for rule in &metadata.matches {
        if super::pattern::matches(root, &rule.pattern, key)? {
            return Ok(RouteDecision::Link {
                pattern: rule.pattern.clone(),
                locator: rule.container.locator.clone(),
                container_uid: rule.container.uid.clone(),
            });
        }
    }
    Ok(RouteDecision::Local)
}

/// Selects a route and calculates a SHA-1 key only when required.
///
/// # Arguments
///
/// * `root` - ConfigurableContainer root.
/// * `metadata` - Ordered current rules.
/// * `key` - Entry key to match.
/// * `data` - Complete prospective entry contents.
///
/// # Returns
///
/// A local route or a data-bearing link route.
///
/// # Errors
///
/// Returns an error when pattern evaluation fails.
pub(super) fn route_data(
    root: &Path,
    metadata: &ConfigRulesMetadata,
    key: &EntryKey,
    data: &Buffer,
) -> Result<DataRoute> {
    Ok(match decide(root, metadata, key)? {
        RouteDecision::Local => DataRoute::Local,
        RouteDecision::Link {
            pattern,
            locator,
            container_uid,
        } => DataRoute::Link {
            pattern,
            locator,
            container_uid,
            target_key: sha1_key(data),
        },
    })
}

/// Calculates the two-level lowercase SHA-1 entry key.
///
/// # Arguments
///
/// * `data` - Complete entry bytes.
///
/// # Returns
///
/// `ab/cd/<remaining 36 hex digits>`.
fn sha1_key(data: &[u8]) -> EntryKey {
    let digest = format!("{:x}", Sha1::digest(data));
    format!("{}/{}/{}", &digest[..2], &digest[2..4], &digest[4..])
}
