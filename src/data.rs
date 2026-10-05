//! Data the user keeps on players and on the server (PLAN.md 8.1, 10.8).
//!
//! - **Extensions**: any `Send + Sync + 'static` value per player and type
//!   ([`Player::insert_extension`](crate::player::Player::insert_extension)). They are not saved
//!   and go when the player leaves.
//! - **Player data**: a serde type registered with
//!   [`ServerBuilder::player_data`](crate::ServerBuilder::player_data). Every player has one value
//!   of it ([`Player::data`](crate::player::Player::data), its [`Default`] for a new player),
//!   saved in the player's record with the rest of the game in the same commit.
//! - **Server data**: a serde type registered with
//!   [`ServerBuilder::server_data`](crate::ServerBuilder::server_data): one value for the server
//!   ([`Context::data`](crate::Context::data)), loaded at start and saved in the same commit.
//!
//! Every value is `Send + Sync`: the tick's output phase reads the players in parallel.
//!
//! Saved values are JSON. A value is written whole or not at all: a value whose JSON is larger
//! than its limit (or does not serialize) is not written, its last saved form stays, and a
//! warning is logged. A saved value that no longer deserializes into its type (a changed type)
//! is never overwritten: a player's reads as `None` and stays as saved, a server value refuses
//! the start. Changes written to systems outside the server file are not undone by a crash
//! (PLAN.md 6).

use std::any::{Any, TypeId};
use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The most player data types (16 values of up to 64 KiB keep a player's record small).
pub const MAX_PLAYER_TYPES: usize = 16;
/// The most server data types.
pub const MAX_SERVER_TYPES: usize = 64;
/// The most extension types a player holds at once.
pub const MAX_EXTENSIONS: usize = 64;
/// The largest saved player data value, in bytes of JSON.
pub const MAX_PLAYER_VALUE_BYTES: usize = 64 << 10;
/// The largest saved server data value, in bytes of JSON.
pub const MAX_SERVER_VALUE_BYTES: usize = 1 << 20;
/// The longest data name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// Why user data was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A data name is empty, longer than [`MAX_NAME_BYTES`], or has a byte other than ASCII
    /// lowercase letters, digits, `_`, `-`, `.` and `:`.
    InvalidName {
        /// The name.
        name: String,
    },
    /// Two player data types or two server data types have the same name.
    DuplicateName {
        /// The name.
        name: String,
    },
    /// A type is registered twice as player data or twice as server data.
    DuplicateType {
        /// The second name it was registered under.
        name: String,
    },
    /// More than [`MAX_PLAYER_TYPES`] player data types or [`MAX_SERVER_TYPES`] server data types.
    TooManyTypes,
    /// The player holds [`MAX_EXTENSIONS`] extension types already.
    TooManyExtensions,
    /// The saved server value does not deserialize into its registered type.
    Undecodable {
        /// The value's name.
        name: String,
    },
}

/// A result with the data [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(f, "{name:?} is not a valid data name"),
            Self::DuplicateName { name } => write!(f, "two data types are named {name:?}"),
            Self::DuplicateType { name } => {
                write!(f, "the type of {name:?} is registered already")
            }
            Self::TooManyTypes => write!(
                f,
                "more than {MAX_PLAYER_TYPES} player or {MAX_SERVER_TYPES} server data types"
            ),
            Self::TooManyExtensions => {
                write!(f, "the player holds {MAX_EXTENSIONS} extensions already")
            }
            Self::Undecodable { name } => {
                write!(f, "the saved value {name:?} does not deserialize")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Checks a data name: 1 to [`MAX_NAME_BYTES`] bytes of `a-z`, `0-9`, `_`, `-`, `.`, `:`.
pub(crate) fn check_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.:".contains(&b));
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidName {
            name: name.to_owned(),
        })
    }
}

/// Why a value has no JSON form within its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonError {
    /// The JSON is longer than the limit.
    TooLarge,
    /// The value does not serialize (or is not of the codec's type).
    Encode,
}

/// The JSON of `value`, failing as soon as it grows past `limit` bytes: the bytes are never
/// allocated beyond the limit.
pub(crate) fn to_json<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> std::result::Result<Vec<u8>, JsonError> {
    let mut out = Capped {
        bytes: Vec::new(),
        limit,
    };
    match serde_json::to_writer(&mut out, value) {
        Ok(()) => Ok(out.bytes),
        Err(e) if e.is_io() => Err(JsonError::TooLarge),
        Err(_) => Err(JsonError::Encode),
    }
}

/// A buffer that refuses to grow past its limit.
struct Capped {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.limit {
            return Err(std::io::Error::other("over the limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A user value as the engine stores it: any type, behind its codec.
pub(crate) type Value = Box<dyn Any + Send + Sync>;

/// How the engine makes, saves and loads a registered type without knowing it.
pub(crate) struct Codec {
    /// The name it is saved under.
    pub(crate) name: String,
    /// The registered type.
    pub(crate) type_id: TypeId,
    default: fn() -> Value,
    encode: fn(&Value, usize) -> std::result::Result<Vec<u8>, JsonError>,
    decode: fn(&[u8]) -> Option<Value>,
}

impl Codec {
    /// The codec of `T` under `name`.
    pub(crate) fn of<T: Default + Serialize + DeserializeOwned + Send + Sync + 'static>(
        name: String,
    ) -> Self {
        Self {
            name,
            type_id: TypeId::of::<T>(),
            default: || Box::new(T::default()),
            encode: |value, limit| {
                let value = value.downcast_ref::<T>().ok_or(JsonError::Encode)?;
                to_json(value, limit)
            },
            decode: |bytes| {
                serde_json::from_slice::<T>(bytes)
                    .ok()
                    .map(|v| Box::new(v) as Value)
            },
        }
    }

    /// A new value: the type's [`Default`].
    pub(crate) fn default_value(&self) -> Value {
        (self.default)()
    }

    /// The JSON of `value`, at most `limit` bytes.
    pub(crate) fn encode(
        &self,
        value: &Value,
        limit: usize,
    ) -> std::result::Result<Vec<u8>, JsonError> {
        (self.encode)(value, limit)
    }

    /// The value saved as `bytes`, if they deserialize into the type.
    pub(crate) fn decode(&self, bytes: &[u8]) -> Option<Value> {
        (self.decode)(bytes)
    }
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codec")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_short_lowercase_ascii() {
        for good in ["kills", "pvp:kills", "a.b-c_9", &"x".repeat(MAX_NAME_BYTES)] {
            assert_eq!(check_name(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "Kills",
            "a b",
            "é",
            "a/b",
            &"x".repeat(MAX_NAME_BYTES + 1),
        ] {
            assert!(
                matches!(check_name(bad), Err(Error::InvalidName { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn json_stops_at_its_limit_without_growing_past_it() {
        let big = vec![7_u32; 1000];
        let full = to_json(&big, usize::MAX).unwrap();
        assert_eq!(to_json(&big, full.len()), Ok(full.clone()));
        assert_eq!(to_json(&big, full.len() - 1), Err(JsonError::TooLarge));
        let codec = Codec::of::<Vec<u32>>("v".to_owned());
        let value: Value = Box::new(big.clone());
        assert_eq!(codec.encode(&value, usize::MAX), Ok(full.clone()));
        let other: Value = Box::new(5_u8);
        assert_eq!(codec.encode(&other, usize::MAX), Err(JsonError::Encode));
        let back = codec.decode(&full).unwrap();
        assert_eq!(back.downcast_ref::<Vec<u32>>(), Some(&big));
        assert!(codec.decode(b"{").is_none());
        assert_eq!(
            codec.default_value().downcast_ref::<Vec<u32>>(),
            Some(&Vec::new())
        );
        // A map with non-string keys does not serialize to JSON.
        let map = std::collections::BTreeMap::from([((1, 2), 3)]);
        assert_eq!(to_json(&map, usize::MAX), Err(JsonError::Encode));
    }
}
