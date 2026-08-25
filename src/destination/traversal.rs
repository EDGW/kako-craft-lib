//! Generic direct and recursive traversal of declared Destination members.

use anyhow::Result;

use super::{DestinationMember, Subcontainer};

/// Lists the Container and Subcontainer members declared below one catalog node.
///
/// # Arguments
///
/// * `node` - Destination root or already resolved Subcontainer to enumerate.
/// * `recursive` - Whether to descend through every declared Subcontainer.
///
/// # Returns
///
/// Both member kinds in stable logical-path order. With `recursive == false`,
/// only direct children are returned; with `true`, descendants are included.
///
/// # Errors
///
/// Returns an error when a node cannot enumerate its declared children or a
/// declared Subcontainer cannot be opened for recursive traversal.
pub fn list_members(node: &dyn Subcontainer, recursive: bool) -> Result<Vec<DestinationMember>> {
    let mut members = Vec::new();
    collect_members(node, recursive, &mut members)?;
    members.sort_by(|left, right| member_path(left).cmp(member_path(right)));
    Ok(members)
}

/// Appends one node's members and optionally visits its Subcontainer children.
///
/// # Arguments
///
/// * `node` - Current catalog node.
/// * `recursive` - Whether Subcontainer children should be opened and visited.
/// * `members` - Accumulator receiving tagged descriptors.
///
/// # Returns
///
/// `Ok(())` after the requested portion of the tree has been appended.
///
/// # Errors
///
/// Returns an enumeration or Subcontainer-opening error from `node` or any
/// recursively visited child.
fn collect_members(
    node: &dyn Subcontainer,
    recursive: bool,
    members: &mut Vec<DestinationMember>,
) -> Result<()> {
    members.extend(
        node.containers()?
            .into_iter()
            .map(DestinationMember::Container),
    );
    let subcontainers = node.subcontainers()?;
    members.extend(
        subcontainers
            .iter()
            .cloned()
            .map(DestinationMember::Subcontainer),
    );
    if recursive {
        for descriptor in subcontainers {
            let child = node.open_subcontainer(&descriptor.name)?;
            collect_members(child.as_ref(), true, members)?;
        }
    }
    Ok(())
}

/// Returns the logical path used to sort a tagged member.
///
/// # Arguments
///
/// * `member` - Container or Subcontainer descriptor.
///
/// # Returns
///
/// A path borrowed from the descriptor.
fn member_path(member: &DestinationMember) -> &str {
    match member {
        DestinationMember::Container(descriptor) => &descriptor.logical_path,
        DestinationMember::Subcontainer(descriptor) => &descriptor.logical_path,
    }
}
