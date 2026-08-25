//! End-to-end container storage, linking, validation, migration, and repair tests.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{
    AddError, BrokenLinkError, CONTAINER_METADATA_FILE, CONTROL_DIR, CheckActionError,
    CheckRepairAction, Container, ContainerMetadata, ContainerWriteGuard, CopyError, EntryKey,
    INCOMING_LINKS_FORMAT_VERSION, LinkCheckKind, LinkContainer, LinkFromError, LinkInfo,
    LinkMetadataMigrationError, LinkToError, LinkUnavailableError, LinkValidationIssueKind,
    LinkValidationRunError, LocalContainer, OUTGOING_LINKS_FORMAT_VERSION, RemoveError,
    RenameError, UnlinkToError, WriteError, WriterError, open_container, open_container_from_json,
};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let path = PathBuf::from("tests").join(Uuid::new_v4().to_string());
        fs::create_dir_all(&path).expect("failed to create isolated test directory");
        Self { path }
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to remove test directory {}: {error}",
                self.path.display()
            );
        }
    }
}

fn metadata_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn write_metadata_json(path: &Path, value: &serde_json::Value) {
    let mut data = serde_json::to_vec_pretty(value).unwrap();
    data.push(b'\n');
    fs::write(path, data).unwrap();
}

fn incoming_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join("links.json")
}

fn outgoing_path(root: &Path) -> PathBuf {
    root.join(CONTROL_DIR).join("outgoing-links.json")
}

#[test]
fn local_container_operations() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let path = directory.join("local");
    let container = LocalContainer::with_logical_name(&path, "local-test")
        .expect("failed to create local container");

    assert_eq!(container.path(), path);
    assert_eq!(container.logical_name(), "local-test");
    assert_eq!(container.kind(), "local");

    let uid = container.uid().expect("failed to get local container UID");
    assert!(Uuid::parse_str(&uid).is_ok(), "UID should be a UUID: {uid}");
    let metadata_path = path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE);
    let metadata = ContainerMetadata::from_file(&metadata_path).unwrap();
    assert_eq!(metadata, container.metadata());
    assert_eq!(metadata.uid, uid);
    assert_eq!(metadata.logical_name, "local-test");
    assert_eq!(metadata.kind, "local");

    let reopened = LocalContainer::new(&path).expect("failed to reopen local container");
    assert_eq!(reopened.uid().unwrap(), uid);
    assert_eq!(reopened.logical_name(), "local-test");

    let key: EntryKey = "nested/entry.txt".into();
    let data = b"local container data".to_vec();
    let mut writer = container.writer().expect("failed to create writer");

    assert!(matches!(
        container.writer(),
        Err(WriterError::ContainerLocked)
    ));
    writer
        .write(&key, data.clone())
        .expect("failed to write entry");
    assert_eq!(writer.link_info(&key).unwrap(), LinkInfo::None);
    assert!(matches!(
        writer.write(&"../outside".into(), Vec::new()),
        Err(WriteError::InvalidEntryKey(_))
    ));
    drop(writer);

    assert_eq!(container.filepath(&key).unwrap(), path.join(&key));
    assert_eq!(container.read(&key).unwrap(), data);
    assert_eq!(container.list().unwrap(), vec![key]);

    let opened = open_container(&path).expect("failed to open container from container.json");
    assert_eq!(opened.metadata(), container.metadata());
    assert_eq!(opened.read(&"nested/entry.txt".into()).unwrap(), data);

    let json = fs::read(metadata_path).unwrap();
    let opened = open_container_from_json(&path, json)
        .expect("failed to open container from container.json bytes");
    assert_eq!(opened.kind(), "local");
}

#[test]
fn local_container_crud_operations() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let container = LocalContainer::new(directory.join("local")).unwrap();
    let first: EntryKey = "first.txt".into();
    let second: EntryKey = "nested/second.txt".into();
    let copy: EntryKey = "copy.txt".into();

    let mut writer = container.writer().unwrap();
    writer.add(&first, b"one".to_vec()).unwrap();
    assert!(matches!(
        writer.add(&first, b"again".to_vec()),
        Err(AddError::EntryExists(key)) if key == first
    ));
    writer.rename(&first, &second).unwrap();
    assert!(matches!(
        writer.rename(&first, &copy),
        Err(RenameError::EntryNotFound(key)) if key == first
    ));
    writer.copy(&second, &copy).unwrap();
    assert!(matches!(
        writer.copy(&second, &copy),
        Err(CopyError::EntryExists(key)) if key == copy
    ));
    writer.remove(&second).unwrap();
    assert!(matches!(
        writer.remove(&second),
        Err(RemoveError::EntryNotFound(key)) if key == second
    ));
    drop(writer);

    assert_eq!(container.list().unwrap(), vec![copy.clone()]);
    assert_eq!(container.read(&copy).unwrap(), b"one");
}

#[test]
fn batch_remove_preflight_prevents_partial_deletion() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let container = LocalContainer::new(directory.join("local")).unwrap();
    let existing: EntryKey = "existing".into();
    let missing: EntryKey = "missing".into();
    let mut writer = container.writer().unwrap();
    writer.write(&existing, b"data".to_vec()).unwrap();
    assert!(matches!(
        writer.remove_many(&[existing.clone(), missing.clone()]),
        Err(RemoveError::EntryNotFound(key)) if key == missing
    ));
    drop(writer);
    assert_eq!(container.read(&existing).unwrap(), b"data");
}

#[test]
fn link_container_operations() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::with_logical_name(&target_path, "target")
        .expect("failed to create target container");
    let linker = LinkContainer::with_logical_name(&linker_path, "linker")
        .expect("failed to create link container");

    assert_eq!(linker.path(), linker_path);
    assert_eq!(linker.logical_name(), "linker");
    assert_eq!(linker.kind(), "link");
    assert!(Uuid::parse_str(&linker.uid().unwrap()).is_ok());
    let common_metadata = linker.metadata();
    assert_eq!(common_metadata.kind, "link");
    assert_eq!(
        ContainerMetadata::load(&linker_path).unwrap(),
        common_metadata
    );

    let target_key: EntryKey = "source/data.txt".into();
    let linker_key: EntryKey = "links/data.txt".into();
    let ordinary_key: EntryKey = "ordinary.txt".into();
    let data = b"linked container data".to_vec();

    {
        let mut writer = target.writer().expect("failed to create target writer");
        writer
            .write(&target_key, data.clone())
            .expect("failed to write target entry");
    }
    {
        let mut writer = linker.writer().expect("failed to create linker writer");
        writer
            .write(&ordinary_key, b"ordinary data".to_vec())
            .expect("failed to write ordinary entry");
    }

    {
        let mut linker_writer = linker.writer().unwrap();
        assert!(matches!(linker.writer(), Err(WriterError::ContainerLocked)));
        linker_writer
            .link_to(&linker_key, &mut target, &target_key, Some(true))
            .expect("failed to create outgoing link");
    }

    assert!(target_path.join(CONTROL_DIR).join("links.json").is_file());
    assert!(
        linker_path
            .join(CONTROL_DIR)
            .join("outgoing-links.json")
            .is_file()
    );
    let outgoing: serde_json::Value = serde_json::from_slice(
        &fs::read(linker_path.join(CONTROL_DIR).join("outgoing-links.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(outgoing["prefer_relative"], true);
    assert_eq!(outgoing["version"], OUTGOING_LINKS_FORMAT_VERSION);
    assert_eq!(
        outgoing["links"][&linker_key]["container_path"],
        "../target"
    );
    assert_eq!(outgoing["links"][&linker_key]["target_key"], target_key);
    assert_eq!(
        outgoing["links"][&linker_key]["container_uid"],
        target.uid().unwrap()
    );
    assert!(
        outgoing["links"][&linker_key]
            .get("target_filename")
            .is_none()
    );
    assert!(
        outgoing["links"][&linker_key]
            .get("prefer_relative")
            .is_none()
    );

    {
        let mut writer = target.writer().unwrap();
        assert!(matches!(
            writer.remove(&target_key),
            Err(RemoveError::EntryIsLinked(key)) if key == target_key
        ));
        assert!(matches!(
            writer.rename(&target_key, &"renamed-target.txt".into()),
            Err(RenameError::EntryIsLinked(key)) if key == target_key
        ));
    }
    {
        let mut writer = linker.writer().unwrap();
        assert!(matches!(
            writer.remove(&linker_key),
            Err(RemoveError::EntryIsLinked(key)) if key == linker_key
        ));
        assert!(matches!(
            writer.rename(&linker_key, &"renamed-link.txt".into()),
            Err(RenameError::EntryIsLinked(key)) if key == linker_key
        ));
    }

    assert_eq!(linker.read(&linker_key).unwrap(), data);
    assert!(
        fs::symlink_metadata(linker.filepath(&linker_key).unwrap())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        linker.list().unwrap(),
        vec![linker_key.clone(), ordinary_key.clone()]
    );

    let target_uid = target.uid().unwrap();
    let linker_uid = linker.uid().unwrap();
    {
        let guard = linker.writer().expect("failed to inspect outgoing link");
        assert_eq!(
            guard.link_info(&linker_key).unwrap(),
            LinkInfo::LinkTo {
                target_key: target_key.clone(),
                container_uid: target_uid,
                container_path: PathBuf::from("../target"),
            }
        );
        guard
            .validate_links(&linker_key, &[&target])
            .expect("outgoing link validation should run");
        assert!(
            guard
                .validate_links(&linker_key, &[&target])
                .unwrap()
                .is_valid()
        );
    }
    {
        let guard = target.writer().expect("failed to inspect incoming link");
        assert_eq!(
            guard.link_info(&target_key).unwrap(),
            LinkInfo::LinkFrom {
                linkers: vec![super::LinkSource {
                    linker_key: linker_key.clone(),
                    linker_uid,
                }],
            }
        );
        guard
            .validate_links(&target_key, &[&linker])
            .expect("incoming link validation should run");
        assert!(
            guard
                .validate_links(&target_key, &[&linker])
                .unwrap()
                .is_valid()
        );
    }

    {
        let mut writer = linker.writer().expect("failed to create linker writer");
        assert!(matches!(
            writer.write(&linker_key, Vec::new()),
            Err(WriteError::EntryIsLink(key)) if key == linker_key
        ));
    }
    assert!(matches!(
        linker
            .writer()
            .unwrap()
            .link_to(&linker_key, &mut target, &target_key, Some(true)),
        Err(LinkToError::Target(LinkFromError::AlreadyLinked))
    ));

    linker
        .writer()
        .unwrap()
        .unlink_to(&linker_key)
        .expect("failed to remove outgoing link");
    assert!(!linker_path.join(&linker_key).exists());
    assert_eq!(
        linker.writer().unwrap().link_info(&linker_key).unwrap(),
        LinkInfo::None
    );
    assert_eq!(
        target.writer().unwrap().link_info(&target_key).unwrap(),
        LinkInfo::None
    );
    assert!(matches!(
        linker.writer().unwrap().unlink_to(&linker_key),
        Err(UnlinkToError::LinkNotFound)
    ));

    let json = fs::read(linker_path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE)).unwrap();
    let opened = open_container_from_json(&linker_path, json).unwrap();
    assert_eq!(opened.kind(), "link");
    assert_eq!(opened.metadata(), common_metadata);
}

#[test]
fn outgoing_link_validation_returns_broken_and_check_reports_it() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target.txt".into();
    let linker_key: EntryKey = "link.txt".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"target".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    fs::remove_file(target_path.join(&target_key)).unwrap();
    let error = linker.read(&linker_key).unwrap_err();
    assert!(error.downcast_ref::<BrokenLinkError>().is_some());
    let error = linker.filepath(&linker_key).unwrap_err();
    assert!(error.downcast_ref::<BrokenLinkError>().is_some());
    fs::remove_file(linker_path.join(&linker_key)).unwrap();
    let issues = linker.writer().unwrap().check(&[]).unwrap();
    assert!(issues.iter().any(|issue| {
        issue.key == linker_key
            && issue.kind == LinkCheckKind::Validation(LinkValidationIssueKind::TargetEntryMissing)
    }));
    assert!(
        issues
            .iter()
            .all(|issue| issue.kind != LinkCheckKind::MissingSymlink)
    );
}

#[test]
fn outgoing_link_copy_supports_multiple_links_to_one_target() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target.txt".into();
    let first: EntryKey = "first.txt".into();
    let second: EntryKey = "second.txt".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"target".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&first, &mut target, &target_key, None)
        .unwrap();
    linker.writer().unwrap().link_copy(&first, &second).unwrap();

    assert_eq!(linker.read(&first).unwrap(), b"target");
    assert_eq!(linker.read(&second).unwrap(), b"target");
    assert_eq!(
        linker.link_list().unwrap(),
        vec![first.clone(), second.clone()]
    );
    let LinkInfo::LinkFrom { linkers } = target.writer().unwrap().link_info(&target_key).unwrap()
    else {
        panic!("expected incoming links");
    };
    assert_eq!(linkers.len(), 2);
    assert!(linkers.iter().any(|link| link.linker_key == first));
    assert!(linkers.iter().any(|link| link.linker_key == second));
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(report.is_valid());
    assert_eq!(report.valid.len(), 2);
    linker.writer().unwrap().unlink_to(&first).unwrap();
    assert_eq!(linker.read(&second).unwrap(), b"target");
    linker.writer().unwrap().unlink_to(&second).unwrap();
    assert_eq!(
        target.writer().unwrap().link_info(&target_key).unwrap(),
        LinkInfo::None
    );
}

#[test]
fn outgoing_link_rename_updates_both_sides_without_losing_the_target() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let old_key: EntryKey = "old".into();
    let new_key: EntryKey = "new".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&old_key, &mut target, &target_key, None)
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_rename(&old_key, &new_key)
        .unwrap();

    assert!(!linker.path().join(&old_key).exists());
    assert_eq!(linker.read(&new_key).unwrap(), b"data");
    let LinkInfo::LinkFrom { linkers } = target.writer().unwrap().link_info(&target_key).unwrap()
    else {
        panic!("renamed reciprocal incoming record is missing");
    };
    assert_eq!(linkers.len(), 1);
    assert_eq!(linkers[0].linker_key, new_key);
    assert!(
        linker
            .writer()
            .unwrap()
            .validate_links(&new_key, &[&target])
            .unwrap()
            .is_valid()
    );
}

#[test]
fn outgoing_link_rename_rolls_back_an_atomic_incoming_rename_on_install_failure() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let old_key: EntryKey = "old".into();
    let new_key: EntryKey = "new".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&old_key, &mut target, &target_key, None)
        .unwrap();

    super::link::fail_outgoing_metadata_write_after(0);
    assert!(
        linker
            .writer()
            .unwrap()
            .link_rename(&old_key, &new_key)
            .is_err()
    );

    assert_eq!(linker.read(&old_key).unwrap(), b"data");
    assert!(fs::symlink_metadata(linker.path().join(&new_key)).is_err());
    let LinkInfo::LinkFrom { linkers } = target.writer().unwrap().link_info(&target_key).unwrap()
    else {
        panic!("rolled-back reciprocal incoming record is missing");
    };
    assert_eq!(linkers.len(), 1);
    assert_eq!(linkers[0].linker_key, old_key);
    assert!(
        linker
            .writer()
            .unwrap()
            .validate_links(&old_key, &[&target])
            .unwrap()
            .is_valid()
    );
}

#[test]
fn partial_link_commit_is_typed_detectable_and_repairable() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();

    super::link::fail_outgoing_metadata_write_after(0);
    super::local::fail_incoming_metadata_write_after(1);
    let error = linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap_err();
    assert!(matches!(error, LinkToError::PartialCommit(_)));
    assert!(!linker.path().join(&linker_key).exists());

    let mut target_writer = target.writer().unwrap();
    let issues = target_writer.check(&[&linker]).unwrap();
    let issue = issues
        .iter()
        .find(|issue| {
            issue.kind == LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord)
        })
        .expect("partial commit should be reported as a missing outgoing record");
    assert!(
        issue
            .actions
            .contains(&CheckRepairAction::AddMissingOutgoingRecord)
    );
    target_writer
        .apply_check_action(
            issue,
            CheckRepairAction::AddMissingOutgoingRecord,
            &[&linker],
        )
        .unwrap();
    drop(target_writer);

    assert_eq!(linker.read(&linker_key).unwrap(), b"data");
    assert!(
        target
            .writer()
            .unwrap()
            .validate_links(&target_key, &[&linker])
            .unwrap()
            .is_valid()
    );
}

#[test]
fn relative_links_survive_moving_their_common_parent() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let original = directory.join("original");
    let target_path = original.join("target");
    let linker_path = original.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, Some(true))
        .unwrap();
    drop(linker);
    drop(target);

    let moved = directory.join("moved");
    fs::rename(&original, &moved).unwrap();
    let moved_target = LocalContainer::new(moved.join("target")).unwrap();
    let moved_linker = LinkContainer::new(moved.join("linker")).unwrap();
    assert_eq!(moved_linker.read(&linker_key).unwrap(), b"data");
    assert!(
        moved_linker
            .writer()
            .unwrap()
            .validate_links(&linker_key, &[&moved_target])
            .unwrap()
            .is_valid()
    );
}

#[test]
fn link_path_override_does_not_change_the_container_default() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let first_target: EntryKey = "first-target".into();
    let second_target: EntryKey = "second-target".into();
    {
        let mut writer = target.writer().unwrap();
        writer.write(&first_target, b"one".to_vec()).unwrap();
        writer.write(&second_target, b"two".to_vec()).unwrap();
    }
    linker.writer().unwrap().set_prefer_relative(false).unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"absolute".into(), &mut target, &first_target, None)
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"relative".into(), &mut target, &second_target, Some(true))
        .unwrap();

    let metadata = metadata_json(&outgoing_path(&linker_path));
    assert_eq!(metadata["prefer_relative"], false);
    assert!(
        Path::new(
            metadata["links"]["absolute"]["container_path"]
                .as_str()
                .unwrap()
        )
        .is_absolute()
    );
    assert_eq!(metadata["links"]["relative"]["container_path"], "../target");
    assert!(
        metadata["links"]["relative"]
            .get("prefer_relative")
            .is_none()
    );
}

#[test]
fn validation_reports_unrelated_records_and_containers_as_ignored() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let unrelated = LocalContainer::new(directory.join("unrelated")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let first: EntryKey = "first.txt".into();
    let second: EntryKey = "second.txt".into();
    target
        .writer()
        .unwrap()
        .write(&first, b"first".to_vec())
        .unwrap();
    target
        .writer()
        .unwrap()
        .write(&second, b"second".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"link-first".into(), &mut target, &first, None)
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"link-second".into(), &mut target, &second, None)
        .unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&first, &[&linker, &unrelated])
        .unwrap();
    assert!(report.is_valid());
    assert_eq!(report.valid.len(), 1);
    assert_eq!(report.ignored_corresponding_records, 1);
    assert_eq!(report.ignored_containers, 1);
}

#[test]
fn validation_detects_uid_impersonation_even_when_keys_match() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(directory.join("linker-x")).unwrap();
    let y = LocalContainer::new(directory.join("container-y")).unwrap();
    let target_key: EntryKey = "entry.txt".into();
    let linker_key: EntryKey = "same-key".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    let links_path = target_path.join(CONTROL_DIR).join("links.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&links_path).unwrap()).unwrap();
    value["links"][&target_key][0]["linker_uid"] = serde_json::json!(y.uid().unwrap());
    fs::write(&links_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(!report.is_valid());
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::LinkerUidMismatch)
    );
    assert_eq!(report.ignored_current_records, 1);

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker, &y])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| { issue.kind == LinkValidationIssueKind::LinkerUidMismatch })
    );
    assert!(
        report
            .broken
            .iter()
            .any(|issue| { issue.kind == LinkValidationIssueKind::MissingOutgoingRecord })
    );
    assert_eq!(report.ignored_current_records, 0);
}

#[test]
fn validation_treats_locked_corresponding_container_as_unavailable() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "entry.txt".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"link".into(), &mut target, &target_key, None)
        .unwrap();

    let _linker_lock = linker.writer().unwrap();
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(report.broken.is_empty());
    assert_eq!(report.unavailable.len(), 1);
    assert_eq!(report.ignored_current_records, 0);
}

#[test]
fn validation_classifies_referenced_invalid_metadata_as_broken() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker_path = directory.join("linker");
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"link".into(), &mut target, &target_key, None)
        .unwrap();
    fs::write(outgoing_path(&linker_path), b"not json").unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::MetadataInvalid)
    );
    assert!(report.unavailable.is_empty());
}

#[test]
fn validation_rejects_duplicate_uid_at_different_paths() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let first_path = directory.join("first");
    let second_path = directory.join("second");
    let first = LocalContainer::new(&first_path).unwrap();
    fs::create_dir_all(second_path.join(CONTROL_DIR)).unwrap();
    fs::copy(
        first_path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE),
        second_path.join(CONTROL_DIR).join(CONTAINER_METADATA_FILE),
    )
    .unwrap();
    let second =
        LocalContainer::from_metadata(&second_path, ContainerMetadata::load(&second_path).unwrap())
            .unwrap();
    let current = LocalContainer::new(directory.join("current")).unwrap();

    assert!(matches!(
        current
            .writer()
            .unwrap()
            .validate_links(&"entry".into(), &[&first, &second]),
        Err(LinkValidationRunError::DuplicateContainerUid { .. })
    ));
}

#[test]
fn validation_rejects_current_container_as_corresponding_input() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let current = LocalContainer::new(directory.join("current")).unwrap();
    assert!(matches!(
        current
            .writer()
            .unwrap()
            .validate_links(&"entry".into(), &[&current]),
        Err(LinkValidationRunError::SelfValidation(_))
    ));
}

#[test]
fn link_validation_detects_another_container() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let another = LocalContainer::new(directory.join("another")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target.txt".into();
    let linker_key: EntryKey = "link.txt".into();

    target
        .writer()
        .unwrap()
        .write(&target_key, b"target".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, Some(false))
        .unwrap();

    let guard = linker.writer().unwrap();
    let report = guard.validate_links(&linker_key, &[&another]).unwrap();
    assert!(report.is_valid());
    assert_eq!(report.valid.len(), 1);
    assert_eq!(report.ignored_current_records, 0);
    assert_eq!(report.ignored_containers, 1);
}

#[test]
fn validation_reports_missing_and_unexpected_reciprocal_records() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    let outgoing_file = outgoing_path(&linker_path);
    let original_outgoing = metadata_json(&outgoing_file);
    let mut without_outgoing = original_outgoing.clone();
    without_outgoing["links"] = serde_json::json!({});
    write_metadata_json(&outgoing_file, &without_outgoing);
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(report.broken.iter().any(|issue| {
        issue.kind == LinkValidationIssueKind::MissingOutgoingRecord
            && issue.linker_key.as_deref() == Some(linker_key.as_str())
    }));

    write_metadata_json(&outgoing_file, &original_outgoing);
    let incoming_file = incoming_path(&target_path);
    let original_incoming = metadata_json(&incoming_file);
    let mut without_incoming = original_incoming.clone();
    without_incoming["links"] = serde_json::json!({});
    without_incoming["count"] = serde_json::json!(0);
    write_metadata_json(&incoming_file, &without_incoming);

    let incoming_view = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        incoming_view
            .broken
            .iter()
            .any(|issue| { issue.kind == LinkValidationIssueKind::UnexpectedOutgoingRecord })
    );
    let outgoing_view = linker
        .writer()
        .unwrap()
        .validate_links(&linker_key, &[&target])
        .unwrap();
    assert!(
        outgoing_view
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::MissingIncomingRecord)
    );

    write_metadata_json(&incoming_file, &original_incoming);
    write_metadata_json(&outgoing_file, &without_outgoing);
    fs::remove_file(linker_path.join(&linker_key)).unwrap();
    let report = linker
        .writer()
        .unwrap()
        .validate_links(&linker_key, &[&target])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::UnexpectedIncomingRecord)
    );
}

#[test]
fn validation_reports_target_key_path_uid_and_symlink_mismatches() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let other_path = directory.join("other");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let other = LocalContainer::new(&other_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, Some(false))
        .unwrap();

    let outgoing_file = outgoing_path(&linker_path);
    let original = metadata_json(&outgoing_file);
    let mut wrong_key = original.clone();
    wrong_key["links"][&linker_key]["target_key"] = serde_json::json!("wrong-target");
    write_metadata_json(&outgoing_file, &wrong_key);
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::TargetKeyMismatch)
    );

    let mut wrong_path = original.clone();
    wrong_path["links"][&linker_key]["container_path"] =
        serde_json::json!(fs::canonicalize(&other_path).unwrap().to_string_lossy());
    write_metadata_json(&outgoing_file, &wrong_path);
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::ContainerPathUidMismatch)
    );
    drop(other);

    write_metadata_json(&outgoing_file, &original);
    fs::remove_file(linker_path.join(&linker_key)).unwrap();
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| { issue.kind == LinkValidationIssueKind::MaterializedSymlinkMissing })
    );
}

#[test]
fn validation_reports_linker_key_mismatch_as_a_set_difference() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let old_key: EntryKey = "old".into();
    let actual_key: EntryKey = "actual".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&old_key, &mut target, &target_key, None)
        .unwrap();
    let file = outgoing_path(&linker_path);
    let mut metadata = metadata_json(&file);
    let outgoing = metadata["links"]
        .as_object_mut()
        .unwrap()
        .remove(&old_key)
        .unwrap();
    metadata["links"]
        .as_object_mut()
        .unwrap()
        .insert(actual_key.clone(), outgoing);
    write_metadata_json(&file, &metadata);
    fs::rename(linker_path.join(&old_key), linker_path.join(&actual_key)).unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(report.broken.iter().any(|issue| {
        issue.kind == LinkValidationIssueKind::LinkerKeyMismatch
            && issue.expected.as_deref() == Some(old_key.as_str())
            && issue.actual.as_deref() == Some(actual_key.as_str())
    }));
    assert!(report.broken.iter().all(|issue| {
        issue.kind != LinkValidationIssueKind::MissingOutgoingRecord
            && issue.kind != LinkValidationIssueKind::UnexpectedOutgoingRecord
    }));
}

#[test]
fn validation_ignores_unrelated_duplicate_incoming_but_reports_relevant_duplicate() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let other_key: EntryKey = "other".into();
    let linker_key: EntryKey = "link".into();
    {
        let mut writer = target.writer().unwrap();
        writer.write(&target_key, b"target".to_vec()).unwrap();
        writer.write(&other_key, b"other".to_vec()).unwrap();
    }
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    let incoming_file = incoming_path(&target_path);
    let mut incoming = metadata_json(&incoming_file);
    let source = incoming["links"][&target_key][0].clone();
    incoming["links"][&other_key] = serde_json::json!([source.clone(), source.clone()]);
    incoming["count"] = serde_json::json!(3);
    write_metadata_json(&incoming_file, &incoming);
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(report.is_valid());
    assert_eq!(report.ignored_current_records, 0);

    incoming["links"][&target_key] = serde_json::json!([source.clone(), source]);
    incoming["links"]
        .as_object_mut()
        .unwrap()
        .remove(&other_key);
    incoming["count"] = serde_json::json!(2);
    write_metadata_json(&incoming_file, &incoming);
    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .broken
            .iter()
            .any(|issue| { issue.kind == LinkValidationIssueKind::DuplicateIncomingRecord })
    );
}

#[test]
fn validation_deduplicates_same_corresponding_path() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker, &linker])
        .unwrap();
    assert!(report.is_valid());
    assert_eq!(report.valid.len(), 1);
    assert_eq!(report.ignored_containers, 0);
}

#[test]
fn validation_can_report_valid_broken_ignored_and_unavailable_together() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let valid = LinkContainer::new(directory.join("valid")).unwrap();
    let broken_path = directory.join("broken");
    let broken = LinkContainer::new(&broken_path).unwrap();
    let locked = LinkContainer::new(directory.join("locked")).unwrap();
    let unrelated = LocalContainer::new(directory.join("unrelated")).unwrap();
    let target_key: EntryKey = "target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    for (container, key) in [
        (&valid, "valid-link"),
        (&broken, "broken-link"),
        (&locked, "locked-link"),
    ] {
        container
            .writer()
            .unwrap()
            .link_to(&key.into(), &mut target, &target_key, None)
            .unwrap();
    }
    let mut broken_metadata = metadata_json(&outgoing_path(&broken_path));
    broken_metadata["links"] = serde_json::json!({});
    write_metadata_json(&outgoing_path(&broken_path), &broken_metadata);
    let _locked_guard = locked.writer().unwrap();

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&valid, &broken, &locked, &unrelated])
        .unwrap();
    assert_eq!(report.valid.len(), 1);
    assert!(
        report
            .broken
            .iter()
            .any(|issue| issue.kind == LinkValidationIssueKind::MissingOutgoingRecord)
    );
    assert_eq!(report.unavailable.len(), 1);
    assert_eq!(report.ignored_containers, 1);
}

#[test]
fn one_corresponding_container_reports_all_valid_and_broken_sources() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let valid_key: EntryKey = "valid".into();
    let broken_key: EntryKey = "broken".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    for linker_key in [&valid_key, &broken_key] {
        linker
            .writer()
            .unwrap()
            .link_to(linker_key, &mut target, &target_key, None)
            .unwrap();
    }
    let outgoing = outgoing_path(&linker_path);
    let mut metadata = metadata_json(&outgoing);
    metadata["links"][&broken_key]["target_key"] = serde_json::json!("wrong-target");
    write_metadata_json(&outgoing, &metadata);

    let report = target
        .writer()
        .unwrap()
        .validate_links(&target_key, &[&linker])
        .unwrap();
    assert!(
        report
            .valid
            .iter()
            .any(|link_match| link_match.linker_key == valid_key)
    );
    assert!(report.broken.iter().any(|issue| {
        issue.kind == LinkValidationIssueKind::TargetKeyMismatch
            && issue.linker_key.as_ref() == Some(&broken_key)
    }));
}

#[test]
fn check_offers_and_applies_specific_symlink_action() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker_path = directory.join("linker");
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();
    fs::remove_file(linker_path.join(&linker_key)).unwrap();

    let mut writer = linker.writer().unwrap();
    let issues = writer.check(&[]).unwrap();
    assert!(matches!(linker.writer(), Err(WriterError::ContainerLocked)));
    let issue = issues
        .iter()
        .find(|issue| issue.kind == LinkCheckKind::MissingSymlink)
        .unwrap();
    assert_eq!(
        issue.actions,
        [
            CheckRepairAction::CreateMissingSymlink,
            CheckRepairAction::Skip
        ]
    );
    let invalid = writer
        .apply_check_action(issue, CheckRepairAction::DeleteUnrecordedSymlink, &[])
        .unwrap_err();
    assert!(invalid.downcast_ref::<CheckActionError>().is_some());
    writer
        .apply_check_action(issue, CheckRepairAction::CreateMissingSymlink, &[])
        .unwrap();
    assert!(matches!(linker.writer(), Err(WriterError::ContainerLocked)));
    assert!(
        writer
            .check(&[])
            .unwrap()
            .iter()
            .all(|issue| issue.kind != LinkCheckKind::MissingSymlink)
    );
}

#[test]
fn check_never_offers_destructive_action_for_locked_target() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&"link".into(), &mut target, &target_key, None)
        .unwrap();

    let _target_lock = target.writer().unwrap();
    let issues = linker.writer().unwrap().check(&[]).unwrap();
    let unavailable = issues
        .iter()
        .find(|issue| issue.kind == LinkCheckKind::Unavailable)
        .unwrap();
    assert_eq!(
        unavailable.actions,
        [CheckRepairAction::RetryUnavailable, CheckRepairAction::Skip]
    );
}

#[test]
fn linked_read_and_path_classify_target_lock_as_unavailable() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();

    let _target_lock = target.writer().unwrap();
    let read_error = linker.read(&linker_key).unwrap_err();
    assert!(read_error.downcast_ref::<LinkUnavailableError>().is_some());
    assert!(read_error.downcast_ref::<BrokenLinkError>().is_none());
    let path_error = linker.filepath(&linker_key).unwrap_err();
    assert!(path_error.downcast_ref::<LinkUnavailableError>().is_some());
}

#[test]
fn check_removes_only_the_selected_stale_incoming_source() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let first_path = directory.join("first");
    let first = LinkContainer::new(&first_path).unwrap();
    let second = LinkContainer::new(directory.join("second")).unwrap();
    let target_key: EntryKey = "target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    first
        .writer()
        .unwrap()
        .link_to(&"first-link".into(), &mut target, &target_key, None)
        .unwrap();
    second
        .writer()
        .unwrap()
        .link_to(&"second-link".into(), &mut target, &target_key, None)
        .unwrap();
    let mut first_metadata = metadata_json(&outgoing_path(&first_path));
    first_metadata["links"] = serde_json::json!({});
    write_metadata_json(&outgoing_path(&first_path), &first_metadata);
    fs::remove_file(first_path.join("first-link")).unwrap();

    let mut writer = target.writer().unwrap();
    let issues = writer.check(&[&first, &second]).unwrap();
    let stale = issues
        .iter()
        .find(|issue| {
            issue.kind == LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord)
        })
        .unwrap();
    writer
        .apply_check_action(
            stale,
            CheckRepairAction::RemoveStaleIncomingRecord,
            &[&first, &second],
        )
        .unwrap();
    let LinkInfo::LinkFrom { linkers } = writer.link_info(&target_key).unwrap() else {
        panic!("second incoming source must remain");
    };
    assert_eq!(linkers.len(), 1);
    assert_eq!(linkers[0].linker_key, "second-link");
}

#[test]
fn check_can_recreate_a_missing_outgoing_record_and_symlink() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();
    let mut metadata = metadata_json(&outgoing_path(&linker_path));
    metadata["links"] = serde_json::json!({});
    write_metadata_json(&outgoing_path(&linker_path), &metadata);
    fs::remove_file(linker_path.join(&linker_key)).unwrap();

    let mut writer = target.writer().unwrap();
    let issues = writer.check(&[&linker]).unwrap();
    let missing = issues
        .iter()
        .find(|issue| {
            issue.kind == LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord)
        })
        .unwrap();
    assert!(
        missing
            .actions
            .contains(&CheckRepairAction::AddMissingOutgoingRecord)
    );
    writer
        .apply_check_action(
            missing,
            CheckRepairAction::AddMissingOutgoingRecord,
            &[&linker],
        )
        .unwrap();
    assert!(
        writer
            .validate_links(&target_key, &[&linker])
            .unwrap()
            .is_valid()
    );
    drop(writer);
    assert_eq!(linker.read(&linker_key).unwrap(), b"data");
}

#[test]
fn stale_outgoing_repair_refuses_to_delete_a_non_symlink_entry() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "target".into();
    let linker_key: EntryKey = "link".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"target".to_vec())
        .unwrap();
    linker
        .writer()
        .unwrap()
        .link_to(&linker_key, &mut target, &target_key, None)
        .unwrap();
    let incoming_file = incoming_path(&target_path);
    let mut incoming = metadata_json(&incoming_file);
    incoming["links"] = serde_json::json!({});
    incoming["count"] = serde_json::json!(0);
    write_metadata_json(&incoming_file, &incoming);
    fs::remove_file(linker_path.join(&linker_key)).unwrap();
    fs::write(linker_path.join(&linker_key), b"ordinary").unwrap();

    let mut writer = linker.writer().unwrap();
    let issues = writer.check(&[&target]).unwrap();
    let missing = issues
        .iter()
        .find(|issue| {
            issue.kind == LinkCheckKind::Validation(LinkValidationIssueKind::MissingIncomingRecord)
        })
        .unwrap();
    assert!(
        missing
            .actions
            .contains(&CheckRepairAction::RemoveStaleOutgoingRecord)
    );
    assert!(
        writer
            .apply_check_action(
                missing,
                CheckRepairAction::RemoveStaleOutgoingRecord,
                &[&target],
            )
            .is_err()
    );
    assert_eq!(
        fs::read(linker_path.join(&linker_key)).unwrap(),
        b"ordinary"
    );
    assert!(matches!(
        writer.link_info(&linker_key).unwrap(),
        LinkInfo::LinkTo { .. }
    ));
}

#[test]
fn outgoing_v1_metadata_migrates_with_backup_under_writer_lock() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let target = LocalContainer::new(&target_path).unwrap();
    let linker = LinkContainer::new(&linker_path).unwrap();
    let target_key: EntryKey = "nested/target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    let file = outgoing_path(&linker_path);
    let old = serde_json::json!({
        "version": 1,
        "links": {
            "old-link": {
                "target_key": target_key,
                "container_uid": target.uid().unwrap(),
                "target_filename": fs::canonicalize(target_path.join("nested/target")).unwrap(),
                "prefer_relative": true
            }
        }
    });
    write_metadata_json(&file, &old);
    let original = fs::read(&file).unwrap();

    assert!(linker.prefer_relative().unwrap());
    let migrated = metadata_json(&file);
    assert_eq!(migrated["version"], OUTGOING_LINKS_FORMAT_VERSION);
    assert_eq!(migrated["prefer_relative"], true);
    assert_eq!(migrated["links"]["old-link"]["container_path"], "../target");
    assert!(
        migrated["links"]["old-link"]
            .get("target_filename")
            .is_none()
    );
    assert!(
        migrated["links"]["old-link"]
            .get("prefer_relative")
            .is_none()
    );
    assert_eq!(
        fs::read(file.with_file_name("outgoing-links.json.v1.backup")).unwrap(),
        original
    );
}

#[test]
fn incoming_v1_single_and_duplicate_records_migrate_to_deduplicated_arrays() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let target = LocalContainer::new(&target_path).unwrap();
    let first: EntryKey = "first".into();
    let second: EntryKey = "second".into();
    {
        let mut writer = target.writer().unwrap();
        writer.write(&first, b"one".to_vec()).unwrap();
        writer.write(&second, b"two".to_vec()).unwrap();
    }
    let file = incoming_path(&target_path);
    let source = serde_json::json!({
        "linker_uid": "linker-uid",
        "linker_key": "link-key"
    });
    let old = serde_json::json!({
        "version": 1,
        "count": 3,
        "links": {
            "first": source,
            "second": [source, source]
        }
    });
    write_metadata_json(&file, &old);
    let original = fs::read(&file).unwrap();

    let writer = target.writer().unwrap();
    let LinkInfo::LinkFrom { linkers } = writer.link_info(&first).unwrap() else {
        panic!("migrated incoming source is missing");
    };
    assert_eq!(linkers.len(), 1);
    drop(writer);
    let migrated = metadata_json(&file);
    assert_eq!(migrated["version"], INCOMING_LINKS_FORMAT_VERSION);
    assert_eq!(migrated["count"], 2);
    assert_eq!(migrated["links"]["first"].as_array().unwrap().len(), 1);
    assert_eq!(migrated["links"]["second"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read(file.with_file_name("links.json.v1.backup")).unwrap(),
        original
    );
}

#[test]
fn failed_outgoing_migration_preserves_original_metadata() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target = LocalContainer::new(directory.join("target")).unwrap();
    let linker_path = directory.join("linker");
    let linker = LinkContainer::new(&linker_path).unwrap();
    let file = outgoing_path(&linker_path);
    let old = serde_json::json!({
        "version": 1,
        "links": {
            "broken": {
                "target_key": "expected-name",
                "container_uid": target.uid().unwrap(),
                "target_filename": directory.join("target/wrong-name"),
                "prefer_relative": true
            }
        }
    });
    write_metadata_json(&file, &old);
    let original = fs::read(&file).unwrap();

    let error = linker.prefer_relative().unwrap_err();
    assert!(error.downcast_ref::<LinkMetadataMigrationError>().is_some());
    assert_eq!(fs::read(&file).unwrap(), original);
    assert!(
        !file
            .with_file_name("outgoing-links.json.v1.backup")
            .exists()
    );
}

#[test]
fn outgoing_migration_backup_failure_preserves_original_metadata() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let target = LocalContainer::new(&target_path).unwrap();
    let linker_path = directory.join("linker");
    let linker = LinkContainer::new(&linker_path).unwrap();
    let file = outgoing_path(&linker_path);
    let old = serde_json::json!({
        "version": 1,
        "prefer_relative": true,
        "links": {
            "link": {
                "target_key": "target",
                "container_uid": target.uid().unwrap(),
                "container_path": fs::canonicalize(&target_path).unwrap()
            }
        }
    });
    write_metadata_json(&file, &old);
    let original = fs::read(&file).unwrap();
    fs::create_dir(file.with_file_name("outgoing-links.json.v1.backup")).unwrap();

    let error = linker.prefer_relative().unwrap_err();
    assert!(error.downcast_ref::<LinkMetadataMigrationError>().is_some());
    assert_eq!(fs::read(&file).unwrap(), original);
}

#[test]
fn incoming_migration_backup_failure_preserves_original_metadata() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let target = LocalContainer::new(&target_path).unwrap();
    let target_key: EntryKey = "target".into();
    target
        .writer()
        .unwrap()
        .write(&target_key, b"data".to_vec())
        .unwrap();
    let file = incoming_path(&target_path);
    let old = serde_json::json!({
        "version": 1,
        "count": 1,
        "links": {
            "target": {
                "linker_uid": "linker-uid",
                "linker_key": "linker-key"
            }
        }
    });
    write_metadata_json(&file, &old);
    let original = fs::read(&file).unwrap();
    fs::create_dir(file.with_file_name("links.json.v1.backup")).unwrap();

    let error = target.writer().unwrap().link_info(&target_key).unwrap_err();
    assert!(error.downcast_ref::<LinkMetadataMigrationError>().is_some());
    assert_eq!(fs::read(&file).unwrap(), original);
}
