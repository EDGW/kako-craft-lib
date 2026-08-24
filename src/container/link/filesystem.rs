use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use uuid::Uuid;

use super::super::{CONTROL_DIR, EntryKey};
use super::metadata::OutgoingLink;

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

pub(super) fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

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

pub(super) fn resolved_container_path(root: &Path, link: &OutgoingLink) -> PathBuf {
    if link.container_path.is_absolute() {
        link.container_path.clone()
    } else {
        root.join(&link.container_path)
    }
}

pub(super) fn target_filename(root: &Path, link: &OutgoingLink) -> PathBuf {
    resolved_container_path(root, link).join(&link.target_key)
}

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
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(super) fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
