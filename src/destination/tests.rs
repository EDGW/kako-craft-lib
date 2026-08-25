//! Tests for Minecraft destination index discovery and path safety.

use std::fs;

use super::{Destination, mc::McDestination};

#[test]
fn lists_root_and_version_mod_containers() {
    let root = tempfile_root();
    fs::create_dir(root.join("libraries")).unwrap();
    fs::create_dir(root.join("assets")).unwrap();
    fs::create_dir_all(root.join("versions/1.19.2/mods")).unwrap();
    fs::create_dir(root.join("versions/1.20.1")).unwrap();

    let destination = McDestination::new(&root).unwrap();
    assert_eq!(
        destination.list().unwrap(),
        vec![
            "assets".to_owned(),
            "libraries".to_owned(),
            "versions:1.19.2:mods".to_owned()
        ]
    );
}

#[test]
fn opens_missing_standard_container_and_indexes_version_path() {
    let root = tempfile_root();
    let destination = McDestination::new(&root).unwrap();
    let libraries = destination.open("libraries").unwrap();
    assert_eq!(libraries.root_path(), root.join("libraries"));
    assert!(root.join("libraries/.kcl/container.json").is_file());

    let mods = destination.open("versions:1.19.2:mods").unwrap();
    assert_eq!(mods.root_path(), root.join("versions/1.19.2/mods"));
    assert!(
        destination
            .list()
            .unwrap()
            .contains(&"versions:1.19.2:mods".to_owned())
    );
}

#[test]
fn rejects_unsafe_or_unsupported_indexes() {
    let root = tempfile_root();
    let destination = McDestination::new(&root).unwrap();
    for index in [
        "",
        "../outside",
        ".kcl",
        "versions",
        "versions:1.19.2:assets",
        "versions::mods",
        "versions:../bad:mods",
        "versions:1.19.2:mods:extra",
    ] {
        assert!(
            destination.container_path(index).is_err(),
            "accepted {index:?}"
        );
    }
}

#[test]
fn rejects_non_directory_minecraft_root() {
    let root = tempfile_root().join("file");
    fs::write(&root, b"not a directory").unwrap();
    assert!(McDestination::new(root).is_err());
}

fn tempfile_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kako-mc-destination-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}
