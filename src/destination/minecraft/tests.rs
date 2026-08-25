//! Focused, self-cleaning tests for the Minecraft Destination catalog.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::container::{Container, ContainerWriteGuard, LinkInfo, LinkSource, LocalContainer};
use crate::destination::Subcontainer;
use crate::locator::{ContainerLocator, resolve_container};

use super::McDestination;

/// Isolated Minecraft root removed when the test scope ends.
struct TestDirectory {
    /// Unique temporary root owned by this test.
    path: PathBuf,
}

impl TestDirectory {
    /// Creates one empty temporary Minecraft root.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kako-minecraft-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).expect("failed to create temporary Minecraft root");
        Self { path }
    }

    /// Returns the root path.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a valid Version directory and manifest.
    fn version(&self, id: &str) {
        let path = self.path.join("versions").join(id);
        fs::create_dir_all(&path).expect("failed to create version directory");
        fs::write(path.join(format!("{id}.json")), b"{}").expect("failed to create manifest");
    }
}

impl Drop for TestDirectory {
    /// Removes the complete isolated root, including metadata and symlinks.
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!("failed to remove {}: {error}", self.path.display());
        }
    }
}

#[test]
fn catalog_and_versions_are_declared_without_scanning_unmanaged_directories() {
    let directory = TestDirectory::new();
    directory.version("1.20.1");
    fs::create_dir_all(directory.path().join("versions/no-manifest"))
        .expect("failed to create invalid version candidate");
    fs::create_dir_all(directory.path().join("logs")).expect("failed to create logs");
    fs::create_dir_all(directory.path().join("saves")).expect("failed to create saves");

    let destination = McDestination::new(directory.path()).unwrap();
    let root_containers = destination.containers().unwrap();
    assert_eq!(
        root_containers
            .iter()
            .map(|descriptor| descriptor.logical_path.as_str())
            .collect::<Vec<_>>(),
        vec!["libraries", "mod-cache"]
    );
    assert_eq!(
        destination
            .subcontainers()
            .unwrap()
            .iter()
            .map(|descriptor| descriptor.logical_path.as_str())
            .collect::<Vec<_>>(),
        vec!["assets", "versions"]
    );

    let assets = destination.open_subcontainer("assets").unwrap();
    assert_eq!(
        assets
            .containers()
            .unwrap()
            .iter()
            .map(|descriptor| descriptor.logical_path.as_str())
            .collect::<Vec<_>>(),
        vec!["assets/indexes", "assets/objects"]
    );
    let libraries = destination.open_container("libraries").unwrap();
    assert_eq!(libraries.kind(), "local");
    assert!(
        directory
            .path()
            .join("libraries/.kcl/container.json")
            .is_file()
    );
    let objects = assets.open_container("objects").unwrap();
    assert_eq!(objects.kind(), "local");
    assert!(
        directory
            .path()
            .join("assets/objects/.kcl/container.json")
            .is_file()
    );
    let versions = destination.open_subcontainer("versions").unwrap();
    let version_descriptors = versions.containers().unwrap();
    assert_eq!(version_descriptors.len(), 1);
    assert_eq!(version_descriptors[0].logical_path, "versions/1.20.1");
    assert_eq!(destination.versions().unwrap().len(), 1);
    assert!(root_containers.iter().all(|descriptor| {
        !descriptor.logical_path.starts_with("logs")
            && !descriptor.logical_path.starts_with("saves")
    }));
}

#[test]
fn versions_use_shared_sha1_mod_cache_and_preserve_local_entries() {
    let directory = TestDirectory::new();
    directory.version("one");
    directory.version("two");
    let destination = McDestination::new(directory.path()).unwrap();
    let versions = destination.versions().unwrap();
    assert_eq!(versions.len(), 2);
    assert!(
        versions
            .iter()
            .all(|version| version.container().kind() == "configurable")
    );

    let bytes = b"shared mod bytes".to_vec();
    for version in &versions {
        let mut writer = version.container().writer().unwrap();
        writer
            .add(&"mods/example.jar".into(), bytes.clone())
            .unwrap();
        writer
            .add(&"config/local.txt".into(), b"local".to_vec())
            .unwrap();
    }

    let first_link = versions[0]
        .container()
        .writer()
        .unwrap()
        .link_info(&"mods/example.jar".into())
        .unwrap();
    let target_key = match first_link {
        LinkInfo::LinkTo { target_key, .. } => target_key,
        other => panic!("expected outgoing link, found {other:?}"),
    };
    let cache = LocalContainer::new(destination.mod_cache_path()).unwrap();
    assert_eq!(cache.list().unwrap(), vec![target_key.clone()]);
    let mut expected_sources = vec![
        LinkSource {
            linker_key: "mods/example.jar".into(),
            linker_uid: versions[0].uid().unwrap(),
        },
        LinkSource {
            linker_key: "mods/example.jar".into(),
            linker_uid: versions[1].uid().unwrap(),
        },
    ];
    expected_sources.sort_by(|left, right| {
        left.linker_uid
            .cmp(&right.linker_uid)
            .then_with(|| left.linker_key.cmp(&right.linker_key))
    });
    assert_eq!(
        cache.writer().unwrap().link_info(&target_key).unwrap(),
        LinkInfo::LinkFrom {
            linkers: expected_sources
        }
    );
    assert_eq!(
        versions[0]
            .container()
            .read(&"config/local.txt".into())
            .unwrap(),
        b"local"
    );

    let mut writer = versions[0].container().writer().unwrap();
    writer
        .rename(&"mods/example.jar".into(), &"mods/renamed.jar".into())
        .unwrap();
    writer
        .copy(&"mods/renamed.jar".into(), &"mods/copied.jar".into())
        .unwrap();
    writer
        .write(&"mods/renamed.jar".into(), b"updated bytes".to_vec())
        .unwrap();
    assert!(matches!(
        writer.link_info(&"mods/renamed.jar".into()).unwrap(),
        LinkInfo::LinkTo { .. }
    ));
    drop(writer);
    assert_eq!(cache.list().unwrap().len(), 2);

    let locator: ContainerLocator = format!("{}:versions/one", directory.path().display())
        .parse()
        .unwrap();
    let opened = resolve_container(&locator, directory.path()).unwrap();
    assert_eq!(opened.kind(), "configurable");
}

#[test]
fn existing_rules_are_preserved_and_bad_cache_uid_is_rejected() {
    let directory = TestDirectory::new();
    directory.version("custom");
    let destination = McDestination::new(directory.path()).unwrap();
    let cache = LocalContainer::new(destination.mod_cache_path()).unwrap();
    let version = destination.versions().unwrap().into_iter().next().unwrap();

    version
        .container()
        .set_rules(vec![crate::container::ConfigRule::sha1(
            "/custom/**",
            "../../:mod-cache".parse().unwrap(),
            cache.uid().unwrap(),
        )])
        .unwrap();
    drop(version);
    let reopened = destination.versions().unwrap().into_iter().next().unwrap();
    assert_eq!(
        reopened
            .container()
            .route_decision(&"mods/not-routed.jar".into())
            .unwrap(),
        crate::container::RouteDecision::Local
    );

    let rules_path = directory
        .path()
        .join("versions/custom/.kcl/link-matches.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&rules_path).unwrap()).unwrap();
    json["matches"][0]["container"]["uid"] = serde_json::Value::String("wrong-uid".to_owned());
    fs::write(&rules_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    assert!(destination.versions().is_err());
}
