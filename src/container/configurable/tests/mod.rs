//! End-to-end configurable routing and lock-release scenarios.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::container::{
    ConfigRule, Container, ContainerWriteGuard, LinkInfo, LocalContainer, WriteError, WriterError,
};

use super::super::ConfigurableContainer;

/// Unique temporary root which treats failed cleanup as a test failure.
struct TestDirectory {
    /// Root below the operating-system temporary directory.
    path: PathBuf,
}

impl TestDirectory {
    /// Creates one isolated test root.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kako-configurable-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).expect("failed to create configurable test root");
        Self { path }
    }

    /// Joins a relative path below this test root.
    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!(
                "failed to remove test root {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Creates a configurable source and local cache with a `/mods/**` SHA-1 rule.
fn configured(directory: &TestDirectory) -> (ConfigurableContainer, LocalContainer) {
    let cache = LocalContainer::new(directory.join("cache")).unwrap();
    let configurable = ConfigurableContainer::new(directory.join("version")).unwrap();
    configurable
        .set_rules(vec![ConfigRule::sha1(
            "/mods/**",
            ":../cache".parse().unwrap(),
            cache.uid().unwrap(),
        )])
        .unwrap();
    (configurable, cache)
}

#[test]
fn automatic_crud_routes_and_preserves_storage_properties() {
    let directory = TestDirectory::new();
    let (configurable, cache) = configured(&directory);
    let data = b"same mod bytes".to_vec();

    let mut writer = configurable.writer().unwrap();
    writer.add(&"notes.txt".into(), b"local".to_vec()).unwrap();
    writer.add(&"mods/one.jar".into(), data.clone()).unwrap();
    writer.add(&"mods/two.jar".into(), data.clone()).unwrap();

    assert!(matches!(
        writer.link_info(&"notes.txt".into()).unwrap(),
        LinkInfo::None
    ));
    let LinkInfo::LinkTo { target_key, .. } = writer.link_info(&"mods/one.jar".into()).unwrap()
    else {
        panic!("matched entry was not linked");
    };
    assert_eq!(
        target_key.split('/').map(str::len).collect::<Vec<_>>(),
        vec![2, 2, 36]
    );
    let LinkInfo::LinkFrom { linkers } = cache.writer().unwrap().link_info(&target_key).unwrap()
    else {
        panic!("cache target has no incoming records");
    };
    assert_eq!(linkers.len(), 2);

    writer
        .rename(&"mods/one.jar".into(), &"outside.jar".into())
        .unwrap();
    assert!(matches!(
        writer.link_info(&"outside.jar".into()).unwrap(),
        LinkInfo::LinkTo { .. }
    ));
    writer
        .copy(&"outside.jar".into(), &"copy.jar".into())
        .unwrap();
    writer.write(&"outside.jar".into(), data.clone()).unwrap();
    assert!(matches!(
        writer.link_info(&"outside.jar".into()).unwrap(),
        LinkInfo::None
    ));
    writer.remove(&"copy.jar".into()).unwrap();
    writer.remove(&"mods/two.jar".into()).unwrap();

    assert!(
        writer
            .add(&".kcl/forbidden".into(), b"internal".to_vec())
            .is_err()
    );
    writer.add(&"mods/broken.jar".into(), data.clone()).unwrap();
    let broken_before = writer.link_info(&"mods/broken.jar".into()).unwrap();
    fs::remove_file(configurable.path().join("mods/broken.jar")).unwrap();
    assert!(
        writer
            .write(&"mods/broken.jar".into(), b"replacement".to_vec())
            .is_err()
    );
    assert_eq!(
        writer.link_info(&"mods/broken.jar".into()).unwrap(),
        broken_before,
        "a rejected broken-link write must preserve outgoing metadata"
    );
    drop(writer);

    assert!(cache.filepath(&target_key).unwrap().is_file());
    assert_eq!(
        cache.list().unwrap().len(),
        1,
        "a rejected broken-link write must not create a replacement cache entry"
    );
    assert!(!configurable.path().join(".kcl/forbidden").exists());
    assert_eq!(configurable.read(&"outside.jar".into()).unwrap(), data);
}

#[test]
fn target_contention_releases_every_lock_acquired_by_the_attempt() {
    let directory = TestDirectory::new();
    let old_cache = LocalContainer::new(directory.join("a-old-cache")).unwrap();
    let configurable = ConfigurableContainer::new(directory.join("m-version")).unwrap();
    let new_cache = LocalContainer::new(directory.join("z-new-cache")).unwrap();
    configurable
        .set_rules(vec![ConfigRule::sha1(
            "/mods/**",
            ":../a-old-cache".parse().unwrap(),
            old_cache.uid().unwrap(),
        )])
        .unwrap();
    configurable
        .writer()
        .unwrap()
        .add(&"mods/locked.jar".into(), b"old".to_vec())
        .unwrap();
    configurable
        .set_rules(vec![ConfigRule::sha1(
            "/mods/**",
            ":../z-new-cache".parse().unwrap(),
            new_cache.uid().unwrap(),
        )])
        .unwrap();

    let _new_cache_lock = new_cache.writer().unwrap();
    let mut writer = configurable.writer().unwrap();
    let error = writer
        .write(&"mods/locked.jar".into(), b"new".to_vec())
        .unwrap_err();
    let WriteError::Other(error) = error else {
        panic!("contention did not preserve the writer-error chain");
    };
    assert!(error.chain().any(|cause| matches!(
        cause.downcast_ref::<WriterError>(),
        Some(WriterError::ContainerLocked)
    )));

    // Stable ordering acquires old-cache and source before finding new-cache locked.
    // The failed configurable writer stays alive but owns none of those partial locks.
    assert!(configurable.writer().is_ok());
    assert!(old_cache.writer().is_ok());
    let LinkInfo::LinkTo { container_uid, .. } = configurable
        .writer()
        .unwrap()
        .link_info(&"mods/locked.jar".into())
        .unwrap()
    else {
        panic!("failed transaction changed the old outgoing relationship");
    };
    assert_eq!(container_uid, old_cache.uid().unwrap());
    assert!(new_cache.list().unwrap().is_empty());
}

#[test]
fn ordered_patterns_choose_first_match_and_rules_validate_target_uid() {
    let directory = TestDirectory::new();
    let first = LocalContainer::new(directory.join("first")).unwrap();
    let second = LocalContainer::new(directory.join("second")).unwrap();
    let configurable = ConfigurableContainer::new(directory.join("version")).unwrap();
    configurable
        .set_rules(vec![
            ConfigRule::sha1(
                "/mods/**",
                ":../first".parse().unwrap(),
                first.uid().unwrap(),
            ),
            ConfigRule::sha1("**", ":../second".parse().unwrap(), second.uid().unwrap()),
        ])
        .unwrap();

    let decision = configurable.route_decision(&"mods/a.jar".into()).unwrap();
    let super::super::RouteDecision::Link { container_uid, .. } = decision else {
        panic!("mods entry did not match a link rule");
    };
    assert_eq!(container_uid, first.uid().unwrap());
    assert!(
        configurable
            .set_rules(vec![ConfigRule::sha1(
                "**",
                ":../first".parse().unwrap(),
                second.uid().unwrap(),
            )])
            .is_err()
    );
}
