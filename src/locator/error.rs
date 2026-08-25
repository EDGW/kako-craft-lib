//! Structured locator parsing failures.

use thiserror::Error;

/// Failure to decode a serialized locator.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LocatorParseError {
    /// The locator has a different number of unescaped fields than its type requires.
    #[error("expected {expected} locator fields, found {actual}")]
    FieldCount {
        /// Number of fields required by the requested locator type.
        expected: usize,
        /// Number of fields found after processing escapes.
        actual: usize,
    },
    /// A backslash was followed by a character other than `:` or `\`.
    #[error("invalid locator escape at byte {index}: \\{character}")]
    InvalidEscape {
        /// Byte position of the backslash in the serialized locator.
        index: usize,
        /// Character following the invalid backslash.
        character: char,
    },
    /// The serialized locator ends with a backslash.
    #[error("locator ends with an incomplete escape at byte {0}")]
    TrailingEscape(usize),
    /// A required locator field is empty.
    #[error("locator field '{0}' cannot be empty")]
    EmptyField(&'static str),
}
