use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{
    AddError, BrokenLinkError, CONTAINER_METADATA_FILE, CONTROL_DIR, Container, ContainerMetadata,
    ContainerWriteGuard, CopyError, EntryKey, LinkCheckKind, LinkContainer, LinkFromError,
    LinkInfo, LinkToError, LinkValidationError, LocalContainer, RemoveError, RenameError,
    UnlinkToError, WriteError, WriterError, open_container, open_container_from_json,
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
            .validate_link(&linker_key, &target)
            .expect("outgoing link should be valid");
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
            .validate_link(&target_key, &linker)
            .expect("incoming link should be valid");
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
    let issues = linker.writer().unwrap().check().unwrap();
    assert!(
        issues
            .iter()
            .any(|issue| { issue.key == linker_key && issue.kind == LinkCheckKind::BrokenTarget })
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
    linker.writer().unwrap().unlink_to(&first).unwrap();
    assert_eq!(linker.read(&second).unwrap(), b"target");
    linker.writer().unwrap().unlink_to(&second).unwrap();
    assert_eq!(
        target.writer().unwrap().link_info(&target_key).unwrap(),
        LinkInfo::None
    );
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
    assert!(matches!(
        guard.validate_link(&linker_key, &another),
        Err(LinkValidationError::ContainerMismatch)
    ));
}
