//! Read-only descriptions of catalog members.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A declared Container member without an opened Container handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerDescriptor {
    /// Direct member name within its parent Subcontainer.
    pub name: String,
    /// Slash-separated logical path from the Destination root.
    pub logical_path: String,
    /// Concrete Container kind used for initialization and validation.
    pub kind: String,
    /// Whether common Container metadata already exists on disk.
    pub initialized: bool,
    /// Filesystem path represented by this catalog member.
    pub filesystem_path: PathBuf,
}

/// A declared Subcontainer member without recursively opening its children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubcontainerDescriptor {
    /// Direct member name within its parent Subcontainer.
    pub name: String,
    /// Slash-separated logical path from the Destination root.
    pub logical_path: String,
}

/// A tagged member used by combined Destination list output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "member_type", rename_all = "kebab-case")]
pub enum DestinationMember {
    /// A concrete Container descriptor.
    Container(ContainerDescriptor),
    /// A logical Subcontainer descriptor.
    Subcontainer(SubcontainerDescriptor),
}
