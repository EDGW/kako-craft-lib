//! Local-entry filesystem traversal and atomic persistence helpers.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use uuid::Uuid;

use super::super::{CONTROL_DIR, EntryKey};

/// Serializes synchronized pretty JSON into an already opened file.
///
/// # Arguments
///
/// * `file` - Writable file handle consumed by the buffered writer.
/// * `value` - Serializable value written with a trailing newline.
///
/// # Returns
///
/// `Ok(())` after buffered bytes are flushed and the file is synchronized.
///
/// # Errors
///
/// Returns an error if serialization, writing, flushing, or synchronization fails.
pub(super) fn write_json_to(file: File, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

/// Persists entry bytes without exposing a partially written destination file.
///
/// # Arguments
///
/// * `root` - Container root whose `.kcl/tmp` directory holds the temporary file.
/// * `path` - Final ordinary-entry filesystem path.
/// * `data` - Complete byte content to store.
/// * `replace` - When `true`, rename over an existing destination; when `false`, hard-link into a
///   destination that must not already exist.
///
/// # Returns
///
/// `Ok(())` after synchronized temporary content becomes visible at `path`.
///
/// # Errors
///
/// Returns an I/O error if directory creation, temporary persistence, replacement, or exclusive
/// installation fails. The temporary file is removed on failure when possible.
pub(super) fn write_entry_atomic(
    root: &Path,
    path: &Path,
    data: &[u8],
    replace: bool,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary_directory = root.join(CONTROL_DIR).join("tmp");
    fs::create_dir_all(&temporary_directory)?;
    let temporary = temporary_directory.join(Uuid::new_v4().to_string());
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        if replace {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
            fs::remove_file(&temporary)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Serializes JSON to a temporary sibling and atomically replaces a metadata file.
///
/// # Arguments
///
/// * `path` - Final metadata-file path to replace.
/// * `value` - Serializable metadata value written as synchronized pretty JSON.
///
/// # Returns
///
/// `Ok(())` after the temporary document is renamed to `path`.
///
/// # Errors
///
/// Returns an error if `path` has no parent or directory creation, serialization,
/// synchronization, or rename fails. The temporary file is removed on failure when possible.
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
        write_json_to(file, value)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("failed to persist {}", path.display()))
}

/// Recursively lists ordinary files below a local container root.
///
/// # Arguments
///
/// * `root` - Container root used to derive keys and exclude its top-level `.kcl` directory.
/// * `directory` - Current directory to traverse; callers initially pass `root`.
/// * `entries` - Output vector populated with root-relative keys for regular files.
///
/// # Returns
///
/// `Ok(())` after every reachable subdirectory has been traversed.
///
/// # Errors
///
/// Returns an error when a directory or entry's file type cannot be read.
pub(super) fn list_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<EntryKey>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
    {
        let entry = entry?;
        if directory == root && entry.file_name() == CONTROL_DIR {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            list_entries(root, &entry.path(), entries)?;
        } else if file_type.is_file() {
            entries.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("entry is below container root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(())
}

/// Removes newly empty ancestor directories without crossing the container root.
///
/// # Arguments
///
/// * `directory` - First candidate directory, usually the parent of a removed or moved entry; a
///   `None` value performs no work.
/// * `root` - Container root that is never removed and bounds the cleanup traversal.
///
/// # Returns
///
/// Returns after reaching `root`, leaving its subtree, or encountering the first non-empty or
/// otherwise unremovable directory. Cleanup errors are intentionally ignored.
pub(super) fn remove_empty_parents(mut directory: Option<&Path>, root: &Path) {
    while let Some(path) = directory {
        if path == root || !path.starts_with(root) {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => directory = path.parent(),
            Err(_) => break,
        }
    }
}
