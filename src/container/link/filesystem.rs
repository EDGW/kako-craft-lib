//! Atomic metadata persistence and symbolic-link filesystem helpers.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use uuid::Uuid;

use super::super::{CONTROL_DIR, EntryKey};
use super::metadata::OutgoingLink;

/// Serializes JSON to a temporary sibling and atomically replaces a metadata file.
///
/// # Arguments
///
/// * `path` - Final metadata-file path to replace.
/// * `value` - Serializable metadata value written as pretty JSON with a trailing newline.
///
/// # Returns
///
/// `Ok(())` after the temporary file is flushed, synchronized, and renamed to `path`.
///
/// # Errors
///
/// Returns an error if `path` has no parent or directory creation, serialization, synchronization,
/// or replacement fails. A created temporary file is removed on failure when possible.
pub(super) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result: Result<()> = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist {}", path.display()))
}

/// Converts a possibly relative path to an absolute path without canonicalizing it.
///
/// # Arguments
///
/// * `path` - Path to preserve when absolute or resolve against the process working directory.
///
/// # Returns
///
/// The original absolute path or the current working directory joined with a relative path.
///
/// # Errors
///
/// Returns an error only when the current working directory is required but cannot be read.
pub(super) fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Selects the path representation persisted for a target container.
///
/// # Arguments
///
/// * `root` - Linking container root used as the base for relative-path calculation.
/// * `target_root` - Target container root to record.
/// * `prefer_relative` - When `true`, use a computable path relative to `root`; when `false`, retain
///   `target_root` exactly.
///
/// # Returns
///
/// A relative target-container path when requested and representable, otherwise `target_root`.
pub(super) fn recorded_container_path(
    root: &Path,
    target_root: &Path,
    prefer_relative: bool,
) -> PathBuf {
    if prefer_relative && let Some(relative) = pathdiff::diff_paths(target_root, root) {
        return relative;
    }
    target_root.to_owned()
}

/// Resolves an outgoing record's stored container path from its owner root.
///
/// # Arguments
///
/// * `root` - Root of the link container owning `link`.
/// * `link` - Outgoing record whose target-container path may be relative or absolute.
///
/// # Returns
///
/// The absolute-form candidate target root: an absolute record unchanged or a relative record
/// joined below `root`.
pub(super) fn resolved_container_path(root: &Path, link: &OutgoingLink) -> PathBuf {
    if link.container_path.is_absolute() {
        link.container_path.clone()
    } else {
        root.join(&link.container_path)
    }
}

/// Resolves the target entry filename described by an outgoing record.
///
/// # Arguments
///
/// * `root` - Root of the link container owning `link`.
/// * `link` - Outgoing record containing the target container path and target entry key.
///
/// # Returns
///
/// The resolved target-container root joined with the target entry key.
pub(super) fn target_filename(root: &Path, link: &OutgoingLink) -> PathBuf {
    resolved_container_path(root, link).join(&link.target_key)
}

/// Chooses the filesystem target text stored in a materialized symbolic link.
///
/// # Arguments
///
/// * `root` - Root of the link container owning the outgoing record.
/// * `link_path` - Filesystem location at which the symbolic link is materialized.
/// * `link` - Outgoing metadata record identifying the target entry.
/// * `prefer_relative` - When `true`, make the symlink target relative to `link_path`'s parent when
///   possible; when `false`, use the resolved target filename directly.
///
/// # Returns
///
/// The relative or direct path text that should be passed to the platform symlink API.
pub(super) fn symlink_target(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> PathBuf {
    let target_filename = target_filename(root, link);
    if prefer_relative
        && let Some(parent) = link_path.parent()
        && let Some(relative) = pathdiff::diff_paths(&target_filename, parent)
    {
        return relative;
    }
    target_filename
}

/// Creates the symbolic link represented by an outgoing metadata record.
///
/// # Arguments
///
/// * `root` - Root of the link container owning the record.
/// * `link_path` - Destination path for the new symbolic link.
/// * `link` - Outgoing record identifying the target container and entry.
/// * `prefer_relative` - Controls whether the symlink target should be relative when possible.
///
/// # Returns
///
/// `Ok(())` after parent directories exist and the platform symlink is created.
///
/// # Errors
///
/// Returns an error if `link_path` has no parent, directories cannot be created, or the symbolic
/// link cannot be created (including when a filesystem entry already occupies `link_path`).
pub(super) fn materialize_symlink(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> Result<()> {
    let parent = link_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", link_path.display()))?;
    fs::create_dir_all(parent)?;
    let target = symlink_target(root, link_path, link, prefer_relative);
    create_file_symlink(&target, link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

/// Verifies that a materialized symbolic link exactly matches its outgoing record.
///
/// # Arguments
///
/// * `root` - Root of the link container owning the record.
/// * `link_path` - Expected symbolic-link filesystem path.
/// * `link` - Outgoing record used to derive the expected target.
/// * `prefer_relative` - Relative-target policy used when the link was materialized.
///
/// # Returns
///
/// `Ok(())` only when `link_path` is readable as a symlink and its stored target equals the expected
/// path byte-for-byte.
///
/// # Errors
///
/// Returns an error when the symlink is missing or unreadable, is not a symlink, or points to a
/// different target.
pub(super) fn verify_materialized_symlink(
    root: &Path,
    link_path: &Path,
    link: &OutgoingLink,
    prefer_relative: bool,
) -> Result<()> {
    let actual = fs::read_link(link_path).with_context(|| {
        format!(
            "outgoing link metadata exists but {} is not a symlink",
            link_path.display()
        )
    })?;
    let expected = symlink_target(root, link_path, link, prefer_relative);
    if actual == expected {
        Ok(())
    } else {
        Err(anyhow!(
            "symlink {} points to {}, metadata requires {}",
            link_path.display(),
            actual.display(),
            expected.display()
        ))
    }
}

/// Recursively collects all symbolic links below a container root.
///
/// # Arguments
///
/// * `root` - Container root used to derive entry keys and exclude its top-level `.kcl` directory.
/// * `directory` - Current directory to traverse; callers initially pass `root`.
/// * `symlinks` - Output map populated with root-relative entry keys and stored symlink targets.
///
/// # Returns
///
/// `Ok(())` after every reachable subdirectory has been scanned.
///
/// # Errors
///
/// Returns an error when a directory entry, file type, or symlink target cannot be read.
pub(super) fn collect_symlinks(
    root: &Path,
    directory: &Path,
    symlinks: &mut BTreeMap<EntryKey, PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
    {
        let entry = entry?;
        if directory == root && entry.file_name() == CONTROL_DIR {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_symlinks(root, &path, symlinks)?;
        } else if file_type.is_symlink() {
            let key = path
                .strip_prefix(root)
                .expect("symlink is below container root")
                .to_string_lossy()
                .into_owned();
            symlinks.insert(key, fs::read_link(&path)?);
        }
    }
    Ok(())
}

#[cfg(unix)]
/// Creates a file symbolic link using the Unix filesystem API.
///
/// # Arguments
///
/// * `target` - Path text stored as the symlink target; it need not exist yet.
/// * `link` - Filesystem path at which to create the symlink.
///
/// # Returns
///
/// `Ok(())` after creation, or the operating-system I/O error from `symlink`.
///
/// # Errors
///
/// Returns the platform I/O error when the symlink cannot be created.
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
/// Creates a file symbolic link using the Windows filesystem API.
///
/// # Arguments
///
/// * `target` - Path text stored as the symlink target; it need not exist yet.
/// * `link` - Filesystem path at which to create the symlink.
///
/// # Returns
///
/// `Ok(())` after creation, or the operating-system I/O error from `symlink_file`.
///
/// # Errors
///
/// Returns the platform I/O error when the symlink cannot be created.
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
