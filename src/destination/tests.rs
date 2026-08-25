//! Focused explicit-catalog and FakeDestination lifecycle tests.

use std::fs;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{CatalogDestination, FakeDestination, Subcontainer};
use crate::container::{Container, LocalContainer};
use crate::locator::{ContainerLocator, ContainerPath, resolve_container, resolve_subcontainer};

/// Isolated filesystem root removed when each test scope ends.
struct TestDirectory {
    /// Unique directory below the process temporary directory.
    path: PathBuf,
}

impl TestDirectory {
    /// Creates one unique empty test root.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kako-destination-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).expect("failed to create isolated test directory");
        Self { path }
    }

    /// Joins one relative path below the test root.
    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }

    /// Returns the test root.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!(
                "failed to remove test directory {}: {error}",
                self.path.display()
            );
        }
    }
}

#[test]
fn catalog_enumerates_only_declared_members_and_resolves_paths() {
    let directory = TestDirectory::new();
    let destination = CatalogDestination::new(directory.join("destination")).unwrap();
    fs::create_dir_all(directory.join("destination/unknown")).unwrap();
    destination
        .add_container("cache", "local", None, false)
        .unwrap();
    destination
        .add_container("versions/one", "local", None, false)
        .unwrap();
    destination.add_subcontainer("cache", true).unwrap();

    assert_eq!(
        destination
            .containers()
            .unwrap()
            .into_iter()
            .map(|member| member.name)
            .collect::<Vec<_>>(),
        vec!["cache"]
    );
    assert_eq!(
        destination
            .subcontainers()
            .unwrap()
            .into_iter()
            .map(|member| member.name)
            .collect::<Vec<_>>(),
        vec!["cache", "versions"]
    );
    let locator: ContainerLocator =
        format!("{}:versions/one", directory.join("destination").display())
            .parse()
            .unwrap();
    let opened = resolve_container(&locator, directory.path()).unwrap();
    assert_eq!(opened.kind(), "local");
    let repeated_separator: ContainerLocator =
        format!("{}:versions//one", directory.join("destination").display())
            .parse()
            .unwrap();
    assert!(resolve_container(&repeated_separator, directory.path()).is_err());
    let cache_path = ContainerPath::new("cache/").unwrap();
    assert!(resolve_subcontainer(&destination, Some(&cache_path)).is_ok());
    assert!(
        directory
            .join("destination/versions/one/.kcl/container.json")
            .is_file()
    );
}

#[test]
fn fake_destination_is_empty_but_opens_existing_container() {
    let directory = TestDirectory::new();
    let container = LocalContainer::new(directory.join("plain")).unwrap();
    let fake = FakeDestination::new(directory.path());
    assert!(fake.containers().unwrap().is_empty());
    assert!(fake.subcontainers().unwrap().is_empty());
    let nested = fake.open_subcontainer("plain").unwrap();
    assert!(nested.containers().unwrap().is_empty());
    assert_eq!(
        fake.open_container("plain").unwrap().uid().unwrap(),
        container.uid().unwrap()
    );
    let locator: ContainerLocator = ":plain".parse().unwrap();
    assert_eq!(
        resolve_container(&locator, directory.path())
            .unwrap()
            .uid()
            .unwrap(),
        container.uid().unwrap()
    );
}
