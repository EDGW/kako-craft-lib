//! Locator value models and string/Serde conversions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::LocatorParseError;
use super::parse::{escape_field, split_fields};

/// Filesystem path locating one Destination root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DestinationLocator {
    /// Unescaped filesystem path retained without lexical normalization.
    raw_path: String,
}

impl DestinationLocator {
    /// Creates a Destination locator from an unescaped filesystem path.
    ///
    /// # Arguments
    ///
    /// * `raw_path` - Relative or absolute Destination-root path.
    ///
    /// # Returns
    ///
    /// A locator retaining the path exactly as supplied.
    ///
    /// # Errors
    ///
    /// Returns [`LocatorParseError::EmptyField`] when the path is empty.
    pub fn new(raw_path: impl Into<String>) -> Result<Self, LocatorParseError> {
        let raw_path = raw_path.into();
        if raw_path.is_empty() {
            return Err(LocatorParseError::EmptyField("destination"));
        }
        Ok(Self { raw_path })
    }

    /// Returns the unescaped, unnormalized filesystem path.
    ///
    /// # Returns
    ///
    /// A borrowed path string valid for the locator's lifetime.
    pub fn as_str(&self) -> &str {
        &self.raw_path
    }
}

/// Logical path to a Container or Subcontainer inside a Destination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerPath {
    /// Unescaped logical path with a trailing slash removed.
    raw: String,
    /// Whether the serialized path explicitly required a Subcontainer.
    require_subcontainer: bool,
}

impl ContainerPath {
    /// Creates a logical member path while preserving a trailing-slash request.
    ///
    /// # Arguments
    ///
    /// * `raw` - Unescaped logical path. A final `/` requires a Subcontainer.
    ///
    /// # Returns
    ///
    /// A path retaining all non-trailing characters without normalization.
    ///
    /// # Errors
    ///
    /// Returns [`LocatorParseError::EmptyField`] for an empty or slash-only path.
    pub fn new(raw: impl Into<String>) -> Result<Self, LocatorParseError> {
        let mut raw = raw.into();
        let require_subcontainer = raw.ends_with('/');
        if require_subcontainer {
            raw.pop();
        }
        if raw.is_empty() {
            return Err(LocatorParseError::EmptyField("container"));
        }
        Ok(Self {
            raw,
            require_subcontainer,
        })
    }

    /// Returns the unescaped logical path without its optional trailing slash.
    ///
    /// # Returns
    ///
    /// A borrowed raw path string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Reports whether the original path ended with `/`.
    ///
    /// # Returns
    ///
    /// `true` when lookup must resolve a Subcontainer.
    pub fn requires_subcontainer(&self) -> bool {
        self.require_subcontainer
    }

    /// Returns nonempty slash-delimited logical path components.
    ///
    /// # Returns
    ///
    /// Borrowed components in lookup order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.raw.split('/').filter(|segment| !segment.is_empty())
    }
}

impl fmt::Display for ContainerPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)?;
        if self.require_subcontainer {
            formatter.write_str("/")?;
        }
        Ok(())
    }
}

impl FromStr for ContainerPath {
    type Err = LocatorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Locator for one Container inside an explicit or implicit Destination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerLocator {
    /// Explicit Destination path, or `None` for a FakeDestination rooted at the caller's pwd.
    destination: Option<DestinationLocator>,
    /// Logical Container path within the selected Destination.
    container: ContainerPath,
}

impl ContainerLocator {
    /// Creates a Container locator from already separated raw values.
    ///
    /// # Arguments
    ///
    /// * `destination` - Explicit Destination path, or `None` for FakeDestination lookup.
    /// * `container` - Logical or FakeDestination-relative Container path.
    ///
    /// # Returns
    ///
    /// A locator containing the supplied values.
    pub fn new(destination: Option<DestinationLocator>, container: ContainerPath) -> Self {
        Self {
            destination,
            container,
        }
    }

    /// Returns the explicit Destination locator, when present.
    ///
    /// # Returns
    ///
    /// `Some` for a nonempty serialized destination field; otherwise `None`.
    pub fn destination(&self) -> Option<&DestinationLocator> {
        self.destination.as_ref()
    }

    /// Returns the logical Container path.
    ///
    /// # Returns
    ///
    /// A borrowed path model.
    pub fn container(&self) -> &ContainerPath {
        &self.container
    }
}

impl fmt::Display for ContainerLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(destination) = &self.destination {
            formatter.write_str(&escape_field(destination.as_str()))?;
        }
        formatter.write_str(":")?;
        formatter.write_str(&escape_field(self.container.as_str()))?;
        if self.container.requires_subcontainer() {
            formatter.write_str("/")?;
        }
        Ok(())
    }
}

impl FromStr for ContainerLocator {
    type Err = LocatorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = split_fields(value, 2)?.into_iter();
        let destination = match fields.next().expect("two fields exist") {
            raw if raw.is_empty() => None,
            raw => Some(DestinationLocator::new(raw)?),
        };
        let container = ContainerPath::new(fields.next().expect("two fields exist"))?;
        Ok(Self::new(destination, container))
    }
}

impl Serialize for ContainerLocator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContainerLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Locator for one entry within a Container.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryLocator {
    /// Container portion of this entry locator.
    container: ContainerLocator,
    /// Unescaped entry key retained without normalization.
    entry: String,
}

impl EntryLocator {
    /// Creates an entry locator from a Container locator and raw entry key.
    ///
    /// # Arguments
    ///
    /// * `container` - Container owning the entry.
    /// * `entry` - Nonempty unescaped entry key.
    ///
    /// # Returns
    ///
    /// A combined locator.
    ///
    /// # Errors
    ///
    /// Returns [`LocatorParseError::EmptyField`] when `entry` is empty.
    pub fn new(
        container: ContainerLocator,
        entry: impl Into<String>,
    ) -> Result<Self, LocatorParseError> {
        let entry = entry.into();
        if entry.is_empty() {
            return Err(LocatorParseError::EmptyField("entry"));
        }
        Ok(Self { container, entry })
    }

    /// Returns the Container portion.
    ///
    /// # Returns
    ///
    /// A borrowed Container locator.
    pub fn container(&self) -> &ContainerLocator {
        &self.container
    }

    /// Returns the raw entry key.
    ///
    /// # Returns
    ///
    /// A borrowed unescaped entry string.
    pub fn entry(&self) -> &str {
        &self.entry
    }
}

impl fmt::Display for EntryLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(destination) = self.container.destination() {
            formatter.write_str(&escape_field(destination.as_str()))?;
        }
        formatter.write_str(":")?;
        formatter.write_str(&escape_field(self.container.container().as_str()))?;
        if self.container.container().requires_subcontainer() {
            formatter.write_str("/")?;
        }
        formatter.write_str(":")?;
        formatter.write_str(&escape_field(&self.entry))
    }
}

impl FromStr for EntryLocator {
    type Err = LocatorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = split_fields(value, 3)?.into_iter();
        let destination = match fields.next().expect("three fields exist") {
            raw if raw.is_empty() => None,
            raw => Some(DestinationLocator::new(raw)?),
        };
        let container = ContainerLocator::new(
            destination,
            ContainerPath::new(fields.next().expect("three fields exist"))?,
        );
        Self::new(container, fields.next().expect("three fields exist"))
    }
}

impl Serialize for EntryLocator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EntryLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
