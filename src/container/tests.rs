use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{
    Container, EntryKey, LinkContainer, LinkFromError, LinkInfo, LinkToError, LinkValidationError,
    LocalContainer, UnlinkToError, WriteError, WriterError,
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
}

#[test]
fn link_container_operations() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let target_path = directory.join("target");
    let linker_path = directory.join("linker");
    let mut target = LocalContainer::with_logical_name(&target_path, "target")
        .expect("failed to create target container");
    let mut linker = LinkContainer::with_logical_name(&linker_path, "linker")
        .expect("failed to create link container");

    assert_eq!(linker.path(), linker_path);
    assert_eq!(linker.logical_name(), "linker");
    assert_eq!(linker.kind(), "link");
    assert!(Uuid::parse_str(&linker.uid().unwrap()).is_ok());

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

    linker
        .link_to(&linker_key, &mut target, &target_key, true)
        .expect("failed to create outgoing link");

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
                linker_key: linker_key.clone(),
                linker_uid,
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
        linker.link_to(&linker_key, &mut target, &target_key, true),
        Err(LinkToError::Target(LinkFromError::AlreadyLinked))
    ));

    linker
        .unlink_to(&linker_key, &mut target)
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
        linker.unlink_to(&linker_key, &mut target),
        Err(UnlinkToError::LinkNotFound)
    ));
}

#[test]
fn link_validation_detects_another_container() {
    crate::tests::init_tracing();
    let directory = TestDirectory::new();
    let mut target = LocalContainer::new(directory.join("target")).unwrap();
    let another = LocalContainer::new(directory.join("another")).unwrap();
    let mut linker = LinkContainer::new(directory.join("linker")).unwrap();
    let target_key: EntryKey = "target.txt".into();
    let linker_key: EntryKey = "link.txt".into();

    target
        .writer()
        .unwrap()
        .write(&target_key, b"target".to_vec())
        .unwrap();
    linker
        .link_to(&linker_key, &mut target, &target_key, false)
        .unwrap();

    let guard = linker.writer().unwrap();
    assert!(matches!(
        guard.validate_link(&linker_key, &another),
        Err(LinkValidationError::ContainerMismatch)
    ));
}
