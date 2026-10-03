//! Bounded JSON helpers for untrusted documents: lists with an element cap and JSON shape checks.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, SeqAccess, Visitor};

/// Deserializes a JSON array of at most `MAX` elements, failing at element `MAX + 1` before it
/// is allocated. `null` is an empty list, as Go decodes it (and Go encodes a nil slice as
/// `null`). Use with `#[serde(deserialize_with = "bounded_vec::<_, _, MAX>")]`.
pub(crate) fn bounded_vec<'de, D, T, const MAX: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "an array of at most {MAX} elements")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
            while let Some(item) = seq.next_element()? {
                if out.len() == MAX {
                    return Err(de::Error::invalid_length(MAX + 1, &self));
                }
                out.push(item);
            }
            Ok(out)
        }
    }

    d.deserialize_any(Bounded::<T, MAX>(PhantomData))
}

/// Whether `data` is one JSON object or `null`, like Go's `json.Unmarshal` into a map. Values are
/// skipped without being stored; `serde_json` skips nesting with a heap stack (one byte per
/// level, bounded by the input), not recursion, so depth cannot overflow the thread stack.
pub(crate) fn is_object_or_null(data: &[u8]) -> bool {
    let first = data.iter().find(|b| !b.is_ascii_whitespace());
    matches!(first, Some(b'{' | b'n')) && serde_json::from_slice::<IgnoredAny>(data).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct List {
        #[serde(deserialize_with = "bounded_vec::<_, _, 3>")]
        items: Vec<u8>,
    }

    #[test]
    fn lists_past_the_cap_are_rejected() {
        let parse = |s: &str| serde_json::from_str::<List>(s).map(|l| l.items);
        assert_eq!(parse(r#"{"items":[1,2,3]}"#).unwrap(), [1, 2, 3]);
        assert_eq!(parse(r#"{"items":[]}"#).unwrap(), [] as [u8; 0]);
        assert_eq!(parse(r#"{"items":null}"#).unwrap(), [] as [u8; 0]);
        assert!(parse(r#"{"items":[1,2,3,4]}"#).is_err());
        assert!(parse(r#"{"items":{}}"#).is_err());
    }

    #[test]
    fn object_checks_follow_go_map_decoding() {
        assert!(is_object_or_null(br#" {"a":[1,{"b":null}]} "#));
        assert!(is_object_or_null(b"null"));
        for bad in [&b""[..], b"[]", b"1", b"\"x\"", b"{", b"{} x", b"nul"] {
            assert!(!is_object_or_null(bad));
        }
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(is_object_or_null(deep.as_bytes()));
        assert!(!is_object_or_null(&deep.as_bytes()[..deep.len() - 1]));
    }
}
