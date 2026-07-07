//! Shared utility types.

use std::fmt;
use std::str::FromStr;

use base58::{FromBase58, ToBase58};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EntityIdParseError {
    #[error("empty id")]
    Empty,
    #[error("invalid base58 character {0:?} at {1}")]
    InvalidBase58(char, usize),
    #[error("id decoded to {0} bytes, expected {UUID_BYTES}")]
    InvalidByteLen(usize),
}

/// Time-ordered, base58-encoded identifier backed by a UUIDv7.
///
/// Serializes as base58. Accepts legacy v4-hex-uuid strings on parse
/// (either hyphenated 8-4-4-4-12 or the unhyphenated 32 hex variant)
/// so existing on-disk sessions resume; the canonical form is base58.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntityId([u8; UUID_BYTES]);

impl EntityId {
    #[allow(clippy::disallowed_methods)]
    pub fn generate() -> Self {
        Self(Uuid::now_v7().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; UUID_BYTES] {
        &self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_base58())
    }
}

impl FromStr for EntityId {
    type Err = EntityIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(EntityIdParseError::Empty);
        }
        if let Ok(u) = Uuid::parse_str(s) {
            return Ok(Self(u.into_bytes()));
        }
        decode_base58(s)
    }
}

fn decode_base58(s: &str) -> Result<EntityId, EntityIdParseError> {
    let bytes = s.from_base58().map_err(|e| match e {
        base58::FromBase58Error::InvalidBase58Character(c, pos) => {
            EntityIdParseError::InvalidBase58(c, pos)
        }
        base58::FromBase58Error::InvalidBase58Length => EntityIdParseError::InvalidByteLen(0),
    })?;
    if bytes.len() != UUID_BYTES {
        return Err(EntityIdParseError::InvalidByteLen(bytes.len()));
    }
    let mut arr = [0u8; UUID_BYTES];
    arr.copy_from_slice(&bytes);
    Ok(EntityId(arr))
}

impl Serialize for EntityId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A session id in its exchangeable string form.
///
/// Opaque above the storage boundary; parsed to [`EntityId`] only at storage
/// ops via [`parse`](Self::parse). Preserves the caller's exact string verbatim
/// (legacy hex ids resume unchanged) so wire echo and client correlation hold.
/// Canonical when self-generated via [`from_entity`](Self::from_entity) (base58).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct WireSessionId(String);

impl WireSessionId {
    pub fn from_entity(id: EntityId) -> Self {
        Self(id.to_string())
    }

    pub fn generate() -> Self {
        Self::from_entity(EntityId::generate())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(&self) -> Result<EntityId, EntityIdParseError> {
        self.0.parse()
    }
}

impl From<EntityId> for WireSessionId {
    fn from(id: EntityId) -> Self {
        Self::from_entity(id)
    }
}

impl fmt::Display for WireSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for WireSessionId {
    type Err = EntityIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<EntityId>()?;
        Ok(Self(s.to_string()))
    }
}

impl<'de> Deserialize<'de> for WireSessionId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SAMPLE_HEX: &str = "01965087-4c71-7f00-8000-000000000000";

    fn parse(s: &str) -> EntityId {
        s.parse().unwrap()
    }

    #[test]
    fn generate_is_v7() {
        let id = EntityId::generate();
        let uuid = Uuid::from_bytes(id.0);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn roundtrip_base58() {
        let id = EntityId::generate();
        let s = id.to_string();
        assert!((21..=22).contains(&s.len()));
        assert_eq!(s.parse::<EntityId>().unwrap(), id);
    }

    #[test_case(SAMPLE_HEX)]
    #[test_case("019650874c717f008000000000000000")]
    fn parses_legacy_and_canonical(s: &str) {
        let expected = EntityId(Uuid::parse_str(SAMPLE_HEX).unwrap().into_bytes());
        assert_eq!(parse(s), expected);
    }

    #[test_case("" => matches Err(EntityIdParseError::Empty))]
    #[test_case("O" => matches Err(EntityIdParseError::InvalidBase58('O', 0)))]
    #[test_case("2j87v4grC" => matches Err(EntityIdParseError::InvalidByteLen(_)))]
    fn rejects_bad(s: &str) -> Result<EntityId, EntityIdParseError> {
        s.parse()
    }

    #[test]
    fn serde_keyed_base58() {
        let id = EntityId::generate();
        let s = serde_json::to_string(&id).unwrap();
        assert!((23..=24).contains(&s.len()));
        let back: EntityId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test_case(SAMPLE_HEX)]
    #[test_case("019650874c717f008000000000000000")]
    fn wire_preserves_caller_string(s: &str) {
        let wire: WireSessionId = s.parse().unwrap();
        assert_eq!(wire.as_str(), s);
        assert_eq!(wire.parse().unwrap(), parse(s));
    }

    #[test]
    fn wire_from_entity_is_canonical_base58() {
        let id = EntityId::generate();
        let wire = WireSessionId::from(id);
        assert_eq!(wire.as_str(), id.to_string());
        assert_eq!(wire.parse().unwrap(), id);
    }
}
