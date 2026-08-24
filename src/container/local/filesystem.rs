use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use uuid::Uuid;

use super::super::{CONTROL_DIR, EntryKey};

pub(super) fn write_json_to(file: File, value: &impl Serialize) -> Result<()> {
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

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
