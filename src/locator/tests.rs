//! Focused table-driven tests for locator syntax and round trips.

use std::str::FromStr;

use super::{ContainerLocator, EntryLocator, LocatorParseError};

#[test]
fn locator_round_trips_escaped_fields() {
    for value in [
        ":./test-container",
        "/games/root:versions/one",
        r"C\:\\Games\\MC:versions/one",
        r"/root/with\:colon:cache",
    ] {
        let parsed = ContainerLocator::from_str(value).unwrap();
        assert_eq!(parsed.to_string(), value);
        let json = serde_json::to_string(&parsed).unwrap();
        assert_eq!(
            serde_json::from_str::<ContainerLocator>(&json).unwrap(),
            parsed
        );
    }

    let entry = EntryLocator::from_str(r"/root:versions/one:mods/a\:b.jar").unwrap();
    assert_eq!(entry.entry(), "mods/a:b.jar");
    assert_eq!(entry.to_string(), r"/root:versions/one:mods/a\:b.jar");
    let entry_json = serde_json::to_string(&entry).unwrap();
    assert_eq!(
        serde_json::from_str::<EntryLocator>(&entry_json).unwrap(),
        entry
    );
}

#[test]
fn locator_rejects_ambiguous_or_invalid_text() {
    for (value, expected) in [
        (
            "plain-path",
            LocatorParseError::FieldCount {
                expected: 2,
                actual: 1,
            },
        ),
        (
            "a:b:c",
            LocatorParseError::FieldCount {
                expected: 2,
                actual: 3,
            },
        ),
        (
            r"a:\q",
            LocatorParseError::InvalidEscape {
                index: 2,
                character: 'q',
            },
        ),
        ("a:b\\", LocatorParseError::TrailingEscape(3)),
        ("a:", LocatorParseError::EmptyField("container")),
    ] {
        assert_eq!(ContainerLocator::from_str(value).unwrap_err(), expected);
    }
}
