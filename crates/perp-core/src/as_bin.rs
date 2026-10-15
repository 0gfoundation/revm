//! type used for serialization

use primitives::{hex::FromHex, Address, U256};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Str used in msgpack
#[derive(Debug, Clone, PartialEq)]
pub struct AsBinStr(pub String);

impl Serialize for AsBinStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0.as_bytes())
    }
}

impl<'de> Deserialize<'de> for AsBinStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        let s = String::from_utf8(bytes).map_err(serde::de::Error::custom)?;
        Ok(AsBinStr(s))
    }
}

impl From<String> for AsBinStr {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AsBinStr {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<Address> for AsBinStr {
    fn from(value: Address) -> Self {
        value.to_string().to_lowercase().into()
    }
}

impl From<AsBinStr> for Address {
    fn from(value: AsBinStr) -> Self {
        Address::from_hex(value.0).unwrap()
    }
}

impl From<U256> for AsBinStr {
    fn from(value: U256) -> Self {
        value.to_string().into()
    }
}

impl From<AsBinStr> for U256 {
    fn from(value: AsBinStr) -> Self {
        U256::from_str_radix(&value.0, 10).unwrap()
    }
}

/// `#[serde(with = "opt_address_bin")]` for an `Option<Address>` field: `None` → msgpack `nil`
/// (1 byte), `Some(a)` → a 20-byte msgpack `bin` (22 bytes) — the same `serde_bytes` encoding
/// [`crate::types::Order::owner`] uses for its raw `[u8; 20]`, while letting the field keep the
/// `Address` type. (perp-core builds `primitives` without its `serde` feature, so `Address` has no
/// `Serialize` of its own here.)
pub mod opt_address_bin {
    use primitives::Address;
    use serde::{Deserializer, Serializer};

    /// `None` → `nil`, `Some(a)` → `bin(20)`.
    pub fn serialize<S: Serializer>(v: &Option<Address>, s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(&v.map(|a| a.0 .0), s)
    }

    /// Inverse of [`serialize`].
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Address>, D::Error> {
        let raw: Option<[u8; 20]> = serde_bytes::deserialize(d)?;
        Ok(raw.map(Address::from))
    }
}
