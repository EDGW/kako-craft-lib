//! Escape-aware locator field encoding and decoding.

use super::LocatorParseError;

/// Escapes one raw locator field for inclusion in a serialized locator.
///
/// # Arguments
///
/// * `raw` - Unescaped field value stored by a locator model.
///
/// # Returns
///
/// A string in which literal colons and backslashes are escaped.
pub(super) fn escape_field(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if matches!(character, ':' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Splits a serialized locator at unescaped colons and decodes its fields.
///
/// # Arguments
///
/// * `value` - Complete serialized locator text.
/// * `expected` - Exact number of fields required by the caller.
///
/// # Returns
///
/// Decoded raw fields in their original order.
///
/// # Errors
///
/// Returns [`LocatorParseError`] for unsupported or incomplete escapes and an
/// unexpected field count.
pub(super) fn split_fields(value: &str, expected: usize) -> Result<Vec<String>, LocatorParseError> {
    let mut fields = vec![String::new()];
    let mut characters = value.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            ':' => fields.push(String::new()),
            '\\' => {
                let Some((_, escaped)) = characters.next() else {
                    return Err(LocatorParseError::TrailingEscape(index));
                };
                if !matches!(escaped, ':' | '\\') {
                    return Err(LocatorParseError::InvalidEscape {
                        index,
                        character: escaped,
                    });
                }
                fields.last_mut().expect("one field exists").push(escaped);
            }
            _ => fields.last_mut().expect("one field exists").push(character),
        }
    }
    if fields.len() != expected {
        return Err(LocatorParseError::FieldCount {
            expected,
            actual: fields.len(),
        });
    }
    Ok(fields)
}
