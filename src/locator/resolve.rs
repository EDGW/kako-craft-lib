//! Locator resolution through explicit and fake Destinations.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::container::{ConfigurableContainer, Container, LinkContainer, LocalContainer};
use crate::destination::{FakeDestination, Subcontainer, open_destination};

use super::{ContainerLocator, ContainerPath};

/// Resolves a Container locator relative to a caller-selected working directory.
///
/// # Arguments
///
/// * `locator` - Parsed explicit-Destination or FakeDestination Container locator.
/// * `pwd` - Filesystem base used for empty and relative destination fields.
///
/// # Returns
///
/// An opened concrete Container selected by the locator.
///
/// # Errors
///
/// Returns an error when the Destination or path cannot be opened, a path
/// component is invalid, or the locator explicitly requires a Subcontainer.
pub fn resolve_container(locator: &ContainerLocator, pwd: &Path) -> Result<Box<dyn Container>> {
    if locator.container().requires_subcontainer() {
        bail!("container locator resolves to a subcontainer: {locator}");
    }
    let Some(destination) = locator.destination() else {
        return FakeDestination::new(pwd).open_container(locator.container().as_str());
    };
    let root = resolve_filesystem_path(pwd, destination.as_str());
    let destination = open_destination(root)?;
    resolve_container_path(destination.as_ref(), locator.container())
}

/// Opens or initializes the Container represented by a locator.
///
/// Empty-destination locators may create filesystem Containers directly.
/// Explicit Destination locators open declared catalog members, whose descriptor
/// controls concrete kind and initialization.
///
/// # Arguments
///
/// * `locator` - Target Container locator.
/// * `pwd` - Working directory for empty or relative destination paths.
/// * `kind` - Requested concrete kind for FakeDestination initialization.
/// * `logical_name` - Optional initial name for a newly created FakeDestination Container.
///
/// # Returns
///
/// An opened or newly initialized Container.
///
/// # Errors
///
/// Returns an error for unsupported kinds, Subcontainer locators, Destination
/// resolution failures, catalog kind mismatch, or Container initialization failures.
pub fn initialize_container(
    locator: &ContainerLocator,
    pwd: &Path,
    kind: &str,
    logical_name: Option<&str>,
) -> Result<Box<dyn Container>> {
    if locator.container().requires_subcontainer() {
        bail!("cannot initialize a Container from a Subcontainer locator: {locator}");
    }
    if locator.destination().is_some() {
        let container = resolve_container(locator, pwd)?;
        if container.kind() != kind {
            bail!(
                "Destination member declares kind '{}', not requested kind '{}'",
                container.kind(),
                kind
            );
        }
        return Ok(container);
    }
    let path = resolve_filesystem_path(pwd, locator.container().as_str());
    let name = logical_name.map(str::to_owned);
    match (kind, name) {
        ("local", Some(name)) => Ok(Box::new(LocalContainer::with_logical_name(path, name)?)),
        ("local", None) => Ok(Box::new(LocalContainer::new(path)?)),
        ("link", Some(name)) => Ok(Box::new(LinkContainer::with_logical_name(path, name)?)),
        ("link", None) => Ok(Box::new(LinkContainer::new(path)?)),
        ("configurable", Some(name)) => Ok(Box::new(ConfigurableContainer::with_logical_name(
            path, name,
        )?)),
        ("configurable", None) => Ok(Box::new(ConfigurableContainer::new(path)?)),
        _ => bail!("unsupported Container kind: {kind}"),
    }
}

/// Resolves a logical Subcontainer path below an opened Destination.
///
/// # Arguments
///
/// * `root` - Destination root node from which traversal starts.
/// * `path` - Optional logical path. `None` returns an adapter for `root`.
///
/// # Returns
///
/// The addressed Subcontainer, or the supplied root represented as a boxed adapter.
///
/// # Errors
///
/// Returns an error for invalid components, missing members, or a Container
/// encountered where a Subcontainer is required.
pub fn resolve_subcontainer(
    root: &dyn Subcontainer,
    path: Option<&ContainerPath>,
) -> Result<Box<dyn Subcontainer>> {
    let Some(path) = path else {
        return Ok(root.boxed_clone());
    };
    validate_logical_path(path)?;
    let mut segments = path.segments().peekable();
    let first = segments
        .next()
        .ok_or_else(|| anyhow::anyhow!("subcontainer path is empty"))?;
    validate_segment(first)?;
    let mut current = root.open_subcontainer(first)?;
    for segment in segments {
        validate_segment(segment)?;
        current = current.open_subcontainer(segment)?;
    }
    Ok(current)
}

/// Traverses a logical path and opens its final Container member.
///
/// # Arguments
///
/// * `root` - Root Subcontainer used for traversal.
/// * `path` - Logical Container path whose final segment is a Container name.
///
/// # Returns
///
/// The opened final Container.
///
/// # Errors
///
/// Returns an error for invalid path components or missing/type-mismatched members.
fn resolve_container_path(
    root: &dyn Subcontainer,
    path: &ContainerPath,
) -> Result<Box<dyn Container>> {
    validate_logical_path(path)?;
    let mut segments = path.segments().peekable();
    let mut current = root.boxed_clone();
    while let Some(segment) = segments.next() {
        validate_segment(segment)?;
        if segments.peek().is_none() {
            return current.open_container(segment);
        }
        current = current.open_subcontainer(segment)?;
    }
    bail!("container path is empty")
}

/// Validates the separator structure of a Destination-logical member path.
///
/// FakeDestination filesystem paths bypass this check because repeated or
/// leading separators may be meaningful to the host filesystem.
///
/// # Arguments
///
/// * `path` - Container or Subcontainer path interpreted inside a real Destination.
///
/// # Returns
///
/// `Ok(())` when every slash-delimited component is nonempty.
///
/// # Errors
///
/// Returns an error for leading or repeated separators, or for an extra slash
/// preceding the one trailing slash represented by [`ContainerPath`].
fn validate_logical_path(path: &ContainerPath) -> Result<()> {
    if path.as_str().split('/').any(str::is_empty) {
        bail!("invalid logical container path: {path}");
    }
    Ok(())
}

/// Resolves a possibly relative filesystem string without lexical normalization.
///
/// # Arguments
///
/// * `pwd` - Base directory for a relative value.
/// * `value` - Raw destination path retained by the locator.
///
/// # Returns
///
/// `value` when absolute, otherwise `pwd.join(value)`.
fn resolve_filesystem_path(pwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        pwd.join(path)
    }
}

/// Validates one logical Destination path component.
///
/// # Arguments
///
/// * `segment` - Candidate catalog member name.
///
/// # Returns
///
/// `Ok(())` for a nonempty ordinary name.
///
/// # Errors
///
/// Returns an error for `.` or `..`, which have filesystem meaning rather
/// than catalog meaning.
fn validate_segment(segment: &str) -> Result<()> {
    if segment.is_empty() || matches!(segment, "." | "..") {
        bail!("invalid logical container path component: {segment:?}");
    }
    Ok(())
}
