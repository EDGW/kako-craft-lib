//! Internal tracing context shared by container implementations.

#[derive(Clone, Debug)]
/// Stable tracing context attached to operations on one concrete container.
pub(crate) struct ContainerLogger {
    /// Compact human-readable label containing the kind and abbreviated UID.
    name: String,
    /// Complete persistent container UID emitted as a structured tracing field.
    uid: String,
    /// Concrete container kind, such as `local` or `link`.
    kind: &'static str,
}

impl ContainerLogger {
    /// Builds tracing context for a concrete container.
    ///
    /// # Arguments
    ///
    /// * `kind` - Static concrete-container kind recorded in every operation span.
    /// * `uid` - Complete persistent container UID; it is also abbreviated in the display name.
    ///
    /// # Returns
    ///
    /// A logger context whose display name contains at most the first six UID characters.
    pub(crate) fn new(kind: &'static str, uid: impl Into<String>) -> Self {
        let uid = uid.into();
        let abbreviated_uid: String = uid.chars().take(6).collect();
        let suffix = if uid.chars().count() > 6 { "..." } else { "" };

        Self {
            name: format!("{kind}[{abbreviated_uid}{suffix}]"),
            uid,
            kind,
        }
    }

    /// Returns the compact `kind[uid...]` label used by tracing output.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns the complete persistent container UID without abbreviation.
    pub(crate) fn uid(&self) -> &str {
        &self.uid
    }

    /// Returns the static concrete-container kind supplied to [`Self::new`].
    pub(crate) fn kind(&self) -> &'static str {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::ContainerLogger;

    #[test]
    fn logger_name_abbreviates_long_uid() {
        let logger = ContainerLogger::new("local", "ae2b13a0-1234");
        assert_eq!(logger.name(), "local[ae2b13...]");
    }

    #[test]
    fn logger_name_keeps_short_uid() {
        let logger = ContainerLogger::new("link", "abc123");
        assert_eq!(logger.name(), "link[abc123]");
    }
}
