use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

/// Session log 中每条 entry 的唯一标识，UUIDv7（时间有序）。
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug, Ord, PartialOrd)]
pub struct EntryId(pub Uuid);

impl EntryId {
    /// Generate a new time-ordered UUIDv7 identifier.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for EntryId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Serialize for EntryId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_id_roundtrip_display_fromstr() {
        let id = EntryId::new();
        let s = id.to_string();
        let id2 = EntryId::from_str(&s).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn entry_id_serde_roundtrip() {
        let id = EntryId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert!(json.starts_with('"'));
        let id2: EntryId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn entry_id_is_time_ordered() {
        let a = EntryId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = EntryId::new();
        assert!(a.0 < b.0);
    }
}
