//! Common ContainerWriteGuard delegation and automatic CRUD entry points.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::container::{
    AddError, Buffer, CheckActionResult, CheckRepairAction, Container, ContainerLinkSnapshot,
    ContainerWriteGuard, CopyError, EntryKey, LinkCheckIssue, LinkFromError, LinkInfo, RemoveError,
    RenameError, UnlinkError, WriteError,
};

use super::super::metadata::{load_rules, rules_path};
use super::super::routing::route_data;
use super::ConfigurableContainerWriteGuard;

impl ContainerWriteGuard for ConfigurableContainerWriteGuard {
    fn container_uid(&self) -> &str {
        &self.uid
    }

    fn link_snapshot(&self) -> Result<ContainerLinkSnapshot> {
        self.source()?.link_snapshot()
    }

    fn filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.source()?.filepath(key)
    }

    fn entry_filepath(&self, key: &EntryKey) -> Result<PathBuf> {
        self.source()?.entry_filepath(key)
    }

    fn read(&self, key: &EntryKey) -> Result<Buffer> {
        self.source()?.read(key)
    }

    fn check(&mut self, corresponding: &[&dyn Container]) -> Result<Vec<LinkCheckIssue>> {
        self.source_mut()?.check(corresponding)
    }

    fn apply_check_action(
        &mut self,
        issue: &LinkCheckIssue,
        action: CheckRepairAction,
        corresponding: &[&dyn Container],
    ) -> Result<CheckActionResult> {
        self.source_mut()?
            .apply_check_action(issue, action, corresponding)
    }

    fn add(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), AddError> {
        let span = container_operation_span!(self.logger, "add");
        let _entered = span.enter();
        match self.source().map_err(AddError::Other)?.link_info(key) {
            Ok(LinkInfo::LinkTo { .. }) => return Err(AddError::EntryIsLink(key.clone())),
            Ok(LinkInfo::LinkFrom { .. }) => return Err(AddError::EntryExists(key.clone())),
            Ok(LinkInfo::None) => {}
            Err(error) => return Err(AddError::Other(error)),
        }
        let path = self
            .source()
            .map_err(AddError::Other)?
            .entry_filepath(key)
            .map_err(AddError::Other)?;
        match fs::symlink_metadata(path) {
            Ok(_) => return Err(AddError::EntryExists(key.clone())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AddError::Other(error.into())),
        }
        let metadata = load_rules(&rules_path(&self.root)).map_err(AddError::Other)?;
        let route = route_data(&self.root, &metadata, key, &data).map_err(AddError::Other)?;
        self.add_routed(key, data, route).map_err(AddError::Other)
    }

    fn write(&mut self, key: &EntryKey, data: Buffer) -> std::result::Result<(), WriteError> {
        let span = container_operation_span!(self.logger, "write");
        let _entered = span.enter();
        let metadata = load_rules(&rules_path(&self.root)).map_err(WriteError::Other)?;
        let route = route_data(&self.root, &metadata, key, &data).map_err(WriteError::Other)?;
        self.write_routed(key, data, route)
            .map_err(WriteError::Other)
    }

    fn remove(&mut self, key: &EntryKey) -> std::result::Result<(), RemoveError> {
        self.remove_automatic(key).map_err(|error| {
            error
                .downcast::<RemoveError>()
                .unwrap_or_else(RemoveError::Other)
        })
    }

    fn rename(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), RenameError> {
        self.preserve_property(from, to, false)
            .map_err(RenameError::Other)
    }

    fn copy(&mut self, from: &EntryKey, to: &EntryKey) -> std::result::Result<(), CopyError> {
        self.preserve_property(from, to, true)
            .map_err(CopyError::Other)
    }

    fn link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), LinkFromError> {
        self.source_mut()
            .map_err(LinkFromError::Other)?
            .link_from(linker_uid, target_key, linker_key)
    }

    fn unlink(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        linker_key: &EntryKey,
    ) -> std::result::Result<(), UnlinkError> {
        self.source_mut()
            .map_err(UnlinkError::Other)?
            .unlink(linker_uid, target_key, linker_key)
    }

    fn rename_link_from(
        &mut self,
        linker_uid: &str,
        target_key: &EntryKey,
        from: &EntryKey,
        to: &EntryKey,
    ) -> std::result::Result<(), LinkFromError> {
        self.source_mut()
            .map_err(LinkFromError::Other)?
            .rename_link_from(linker_uid, target_key, from, to)
    }

    fn repair_add_outgoing(
        &mut self,
        linker_key: &EntryKey,
        target_container_uid: &str,
        target_container_path: &Path,
        target_key: &EntryKey,
    ) -> Result<()> {
        self.source_mut()?.repair_add_outgoing(
            linker_key,
            target_container_uid,
            target_container_path,
            target_key,
        )
    }

    fn repair_remove_outgoing(&mut self, linker_key: &EntryKey) -> Result<()> {
        self.source_mut()?.repair_remove_outgoing(linker_key)
    }

    fn link_info(&self, key: &EntryKey) -> Result<LinkInfo> {
        self.source()?.link_info(key)
    }

    fn has_link_from(
        &self,
        target_key: &EntryKey,
        linker_uid: &str,
        linker_key: &EntryKey,
    ) -> Result<bool> {
        self.source()?
            .has_link_from(target_key, linker_uid, linker_key)
    }
}
