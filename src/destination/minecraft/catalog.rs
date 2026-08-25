//! Fixed Minecraft managed catalog descriptors and node dispatch.

use std::path::Path;

use anyhow::Result;

use crate::container::{Container, LocalContainer};
use crate::destination::{ContainerDescriptor, SubcontainerDescriptor};

use super::destination::{is_initialized, node_path, open_managed_version, version_names};

/// One known logical Minecraft Subcontainer node.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ManagedNode {
    /// The Minecraft root.
    Root,
    /// The `assets/` Subcontainer.
    Assets,
    /// The dynamic `versions/` Subcontainer.
    Versions,
}

/// Lists direct managed Containers for a Minecraft node.
pub(crate) fn node_containers(root: &Path, node: ManagedNode) -> Result<Vec<ContainerDescriptor>> {
    let mut descriptors = match node {
        ManagedNode::Root => vec![
            descriptor(root, "libraries", "libraries", "local"),
            descriptor(root, "mod-cache", ".kcl/mod-cache", "local"),
        ],
        ManagedNode::Assets => vec![
            descriptor(root, "assets/objects", "assets/objects", "local"),
            descriptor(root, "assets/indexes", "assets/indexes", "local"),
        ],
        ManagedNode::Versions => version_names(root)?
            .into_iter()
            .map(|name| {
                let path = root.join("versions").join(&name);
                ContainerDescriptor {
                    name: name.clone(),
                    logical_path: format!("versions/{name}"),
                    kind: "configurable".to_owned(),
                    initialized: is_initialized(&path),
                    filesystem_path: path,
                }
            })
            .collect(),
    };
    descriptors.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    Ok(descriptors)
}

/// Lists direct managed Subcontainers for a Minecraft node.
pub(crate) fn node_subcontainers(
    _root: &Path,
    node: ManagedNode,
) -> Result<Vec<SubcontainerDescriptor>> {
    Ok(match node {
        ManagedNode::Root => vec![
            SubcontainerDescriptor {
                name: "assets".to_owned(),
                logical_path: "assets".to_owned(),
            },
            SubcontainerDescriptor {
                name: "versions".to_owned(),
                logical_path: "versions".to_owned(),
            },
        ],
        ManagedNode::Assets | ManagedNode::Versions => Vec::new(),
    })
}

/// Opens or initializes a direct managed Container.
pub(crate) fn open_node_container(
    root: &Path,
    node: ManagedNode,
    name: &str,
) -> Result<Box<dyn Container>> {
    if matches!(node, ManagedNode::Versions) {
        return open_managed_version(root, name);
    }
    let path = node_path(root, node, name)?;
    Ok(Box::new(LocalContainer::with_logical_name(
        path,
        match node {
            ManagedNode::Root => name.to_owned(),
            ManagedNode::Assets => format!("assets/{name}"),
            ManagedNode::Versions => unreachable!(),
        },
    )?))
}

/// Builds one fixed-container descriptor.
fn descriptor(
    root: &Path,
    logical_path: &str,
    filesystem_path: &str,
    kind: &str,
) -> ContainerDescriptor {
    let name = logical_path.rsplit('/').next().unwrap_or(logical_path);
    ContainerDescriptor {
        name: name.to_owned(),
        logical_path: logical_path.to_owned(),
        kind: kind.to_owned(),
        initialized: is_initialized(&root.join(filesystem_path)),
        filesystem_path: root.join(filesystem_path),
    }
}
