#[derive(Clone, Debug)]
pub(crate) struct ContainerLogger {
    name: String,
    uid: String,
    kind: &'static str,
}

impl ContainerLogger {
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

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn uid(&self) -> &str {
        &self.uid
    }

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
