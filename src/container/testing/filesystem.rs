//! JSON metadata mutation, path, and symbolic-link helpers for generated fixtures.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::broken::{INCOMING_FILE, LINKER_KEY, OUTGOING_FILE, TARGET_KEY};
use crate::container::CONTROL_DIR;

/// Resolves a link container's outgoing metadata path.
///
/// # Arguments
///
/// * `root` - Link-container root containing the `.kcl` control directory.
///
/// # Returns
///
/// `root/.kcl/outgoing-links.json` without accessing the filesystem.
pub(super) fn outgoing_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join(OUTGOING_FILE)
}

/// Resolves a local container's incoming metadata path.
///
/// # Arguments
///
/// * `root` - Local or link-container root containing the `.kcl` control directory.
///
/// # Returns
///
/// `root/.kcl/links.json` without accessing the filesystem.
pub(super) fn incoming_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join(INCOMING_FILE)
}

/// Removes the generated outgoing record while retaining reciprocal target metadata.
///
/// # Arguments
///
/// * `linker` - Link-container root whose `link.txt` outgoing map entry is removed.
///
/// # Returns
///
/// `Ok(())` after the modified outgoing JSON is synchronized to disk.
///
/// # Errors
///
/// Returns an error if metadata cannot be opened, parsed as the expected object, or persisted.
pub(super) fn remove_outgoing(linker: &Path) -> Result<()> {
    let path = outgoing_path(linker);
    let mut metadata = read_json(&path)?;
    object_mut(&mut metadata, "links")?.remove(LINKER_KEY);
    write_json(&path, &metadata)
}

/// Removes the generated reciprocal incoming record while retaining outgoing metadata.
///
/// # Arguments
///
/// * `target` - Target-container root whose `target.txt` incoming map entry is removed.
///
/// # Returns
///
/// `Ok(())` after the link map is changed, its cached count is set to zero, and JSON is synchronized.
///
/// # Errors
///
/// Returns an error if metadata cannot be opened, parsed as the expected object, or persisted.
pub(super) fn remove_incoming(target: &Path) -> Result<()> {
    let path = incoming_path(target);
    let mut metadata = read_json(&path)?;
    object_mut(&mut metadata, "links")?.remove(TARGET_KEY);
    metadata["count"] = Value::from(0_u64);
    write_json(&path, &metadata)
}

/// Mutates the single generated outgoing record and persists the document.
///
/// # Arguments
///
/// * `linker` - Link-container root whose `link.txt` outgoing record is selected.
/// * `update` - One-shot callback that mutates the selected JSON record and may reject it with an
///   error.
///
/// # Returns
///
/// `Ok(())` after the callback succeeds and the complete JSON document is synchronized.
///
/// # Errors
///
/// Returns an error if metadata cannot be opened or parsed, the generated record is absent, the
/// callback fails, or the updated document cannot be persisted.
pub(super) fn update_outgoing_record(
    linker: &Path,
    update: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let path = outgoing_path(linker);
    let mut metadata = read_json(&path)?;
    let record = metadata["links"]
        .get_mut(LINKER_KEY)
        .ok_or_else(|| anyhow!("generated outgoing record is missing"))?;
    update(record)?;
    write_json(&path, &metadata)
}

/// Mutates the single generated incoming record and persists the document.
///
/// # Arguments
///
/// * `target` - Target-container root whose first `target.txt` incoming record is selected.
/// * `update` - One-shot callback that mutates the selected JSON record and may reject it with an
///   error.
///
/// # Returns
///
/// `Ok(())` after the callback succeeds and the complete JSON document is synchronized.
///
/// # Errors
///
/// Returns an error if metadata cannot be opened or parsed, the record array is absent or empty, the
/// callback fails, or the updated document cannot be persisted.
pub(super) fn update_incoming_record(
    target: &Path,
    update: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let path = incoming_path(target);
    let mut metadata = read_json(&path)?;
    let record = metadata["links"][TARGET_KEY]
        .as_array_mut()
        .and_then(|records| records.first_mut())
        .ok_or_else(|| anyhow!("generated incoming record is missing"))?;
    update(record)?;
    write_json(&path, &metadata)
}

/// Returns a named JSON object field as a mutable map.
///
/// # Arguments
///
/// * `value` - JSON document containing the selected top-level field.
/// * `field` - Field name whose value must be a JSON object.
///
/// # Returns
///
/// A mutable map borrowed from `value` for the same lifetime.
///
/// # Errors
///
/// Returns an error when `field` is absent or its value is not an object.
pub(super) fn object_mut<'a>(
    value: &'a mut Value,
    field: &str,
) -> Result<&'a mut serde_json::Map<String, Value>> {
    value[field]
        .as_object_mut()
        .ok_or_else(|| anyhow!("generated metadata field '{field}' is not an object"))
}

/// Reads one JSON metadata document.
///
/// # Arguments
///
/// * `path` - Exact metadata-file path to open and parse.
///
/// # Returns
///
/// The complete parsed JSON value.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or does not contain valid JSON.
pub(super) fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .with_context(|| format!("failed to parse {}", path.display()))
}

/// Replaces one generated JSON document with pretty, synchronized content.
///
/// # Arguments
///
/// * `path` - Existing metadata-file path to truncate and rewrite.
/// * `value` - Complete JSON document serialized with indentation and a trailing newline.
///
/// # Returns
///
/// `Ok(())` after serialization, flushing, and file synchronization complete.
///
/// # Errors
///
/// Returns an error if the existing file cannot be opened for writing, serialization or writing
/// fails, or buffered bytes cannot be flushed and synchronized.
pub(super) fn write_json(path: &Path, value: &Value) -> Result<()> {
    let file = OpenOptions::new().write(true).truncate(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

/// Converts a possibly relative fixture path to an absolute path without requiring it to exist.
///
/// # Arguments
///
/// * `path` - Path to retain when absolute or resolve against the process working directory.
///
/// # Returns
///
/// The original absolute path or the current working directory joined with a relative path.
///
/// # Errors
///
/// Returns an error only when `path` is relative and the current working directory cannot be read.
pub(super) fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Removes a symbolic link without following it.
///
/// # Arguments
///
/// * `path` - Expected symbolic-link filesystem path to inspect and remove.
///
/// # Returns
///
/// `Ok(())` after the directory entry itself is removed without touching its target.
///
/// # Errors
///
/// Returns an error if the path cannot be inspected, is not a symbolic link, or cannot be removed.
pub(super) fn remove_symlink(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect generated symlink {}", path.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!("generated path is not a symlink: {}", path.display());
    }
    fs::remove_file(path).with_context(|| format!("failed to remove symlink {}", path.display()))
}

/// Creates a file symbolic link using the Unix filesystem API.
///
/// # Arguments
///
/// * `target` - Path text stored in the new symbolic link; the target need not exist.
/// * `link` - New symbolic-link filesystem path.
///
/// # Returns
///
/// `Ok(())` after the Unix symlink is created.
///
/// # Errors
///
/// Returns the contextualized operating-system error when creation fails.
#[cfg(unix)]
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| {
        format!(
            "failed to create generated symlink {} -> {}",
            link.display(),
            target.display()
        )
    })
}

/// Creates a file symbolic link using the Windows filesystem API.
///
/// # Arguments
///
/// * `target` - Path text stored in the new symbolic link; the target need not exist.
/// * `link` - New symbolic-link filesystem path.
///
/// # Returns
///
/// `Ok(())` after the Windows file symlink is created.
///
/// # Errors
///
/// Returns the contextualized operating-system error when creation fails.
#[cfg(windows)]
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link).with_context(|| {
        format!(
            "failed to create generated symlink {} -> {}",
            link.display(),
            target.display()
        )
    })
}
