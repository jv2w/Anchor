//! JWS compact serialization (RFC 7515): parsing with size caps, ES384 signing, and detached
//! ES384 signatures.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use super::base64::RAW_URL;
use super::{Error, PublicKey, Result, ServerKey};

/// Longest header segment in base64 text. A Bedrock header holds `alg`, `kid`, `typ`, `x5t` or a
/// P-384 `x5u` (160 characters), well under 1 KiB.
const MAX_HEADER_TEXT: usize = 2048;
/// Longest signature segment in base64 text: an RSA-4096 signature is 683 characters.
const MAX_SIGNATURE_TEXT: usize = 1024;

/// The longest token text [`parse`] accepts with `max_payload`: the header and signature caps,
/// the two dots, and `max_payload` bytes in unpadded base64.
pub(crate) const fn max_text(max_payload: usize) -> usize {
    MAX_HEADER_TEXT
        .saturating_add(max_payload.saturating_mul(4).div_ceil(3))
        .saturating_add(MAX_SIGNATURE_TEXT)
        .saturating_add(2)
}

/// A JWS signature algorithm. Everything but ES384 and RS256 is [`Algorithm::Other`] and is never
/// verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum Algorithm {
    /// ECDSA P-384 with SHA-384, signature `r || s` (96 bytes).
    #[serde(rename = "ES384")]
    Es384,
    /// RSASSA-PKCS1-v1_5 with SHA-256.
    #[serde(rename = "RS256")]
    Rs256,
    /// Any other value, including `none` and the HMAC algorithms.
    #[serde(other)]
    Other,
}

/// The protected header fields Bedrock uses.
#[derive(Debug, Deserialize)]
pub(crate) struct Header {
    pub(crate) alg: Algorithm,
    #[serde(default)]
    pub(crate) kid: Option<String>,
    #[serde(default)]
    pub(crate) x5u: Option<String>,
    /// Critical extensions are not supported, so their presence fails parsing (as in go-jose).
    #[serde(default)]
    crit: Option<IgnoredAny>,
}

/// A parsed, not yet verified, compact JWS.
#[derive(Debug)]
pub(crate) struct Jws<'a> {
    pub(crate) header: Header,
    /// `header '.' payload`, the bytes the signature covers.
    pub(crate) signing_input: &'a [u8],
    pub(crate) payload: Vec<u8>,
    pub(crate) signature: Vec<u8>,
}

/// Splits and decodes a compact JWS. The payload may be at most `max_payload` bytes after
/// decoding; nothing larger is allocated.
pub(crate) fn parse(token: &str, max_payload: usize) -> Result<Jws<'_>> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::Jws("not three segments"));
    };
    if header.len() > MAX_HEADER_TEXT || signature.len() > MAX_SIGNATURE_TEXT {
        return Err(Error::TooLarge);
    }
    let signed_len = header.len() + 1 + payload.len();
    let header_json = RAW_URL.decode(header, MAX_HEADER_TEXT)?;
    let header: Header = serde_json::from_slice(&header_json).map_err(Error::Json)?;
    if header.crit.is_some() {
        return Err(Error::Jws("critical header"));
    }
    let signing_input = token
        .as_bytes()
        .get(..signed_len)
        .ok_or(Error::Jws("not three segments"))?;
    Ok(Jws {
        header,
        signing_input,
        payload: RAW_URL.decode(payload, max_payload)?,
        signature: RAW_URL.decode(signature, MAX_SIGNATURE_TEXT)?,
    })
}

#[derive(Serialize)]
struct Es384Header<'a> {
    alg: &'static str,
    x5u: &'a str,
}

/// Signs `claims` with ES384, the header naming the key in `x5u` (base64 SPKI), as gophertunnel's
/// server handshake does.
pub(crate) fn sign_es384(key: &ServerKey, claims: &impl Serialize) -> Result<String> {
    let header = serde_json::to_vec(&Es384Header {
        alg: "ES384",
        x5u: key.public_key_base64(),
    })
    .map_err(Error::Json)?;
    let payload = serde_json::to_vec(claims).map_err(Error::Json)?;
    sign_compact(key, &header, &payload)
}

/// Signs `header` and `payload` bytes as given with ES384, for tokens whose JSON layout is fixed
/// byte for byte. The header must name `ES384`.
pub(crate) fn sign_compact(key: &ServerKey, header: &[u8], payload: &[u8]) -> Result<String> {
    let mut token = RAW_URL.encode(header);
    token.push('.');
    RAW_URL.encode_into(payload, &mut token);
    let signature = key.sign_es384(token.as_bytes())?;
    token.push('.');
    RAW_URL.encode_into(&signature, &mut token);
    Ok(token)
}

/// The protected header of a detached signature, as go-jose and BDS write it.
const DETACHED_HEADER: &[u8] = br#"{"alg":"ES384"}"#;

/// An ES384 JWS of `payload` with the payload segment left empty (RFC 7515 appendix F):
/// `header..signature`.
pub(crate) fn sign_detached(key: &ServerKey, payload: &[u8]) -> Result<String> {
    let token = sign_compact(key, DETACHED_HEADER, payload)?;
    let (header, rest) = token.split_once('.').ok_or(Error::Internal)?;
    let (_, signature) = rest.split_once('.').ok_or(Error::Internal)?;
    Ok(format!("{header}..{signature}"))
}

/// Verifies a detached JWS of `payload` with `key`: the payload segment must be empty and the
/// algorithm ES384; any other algorithm fails (go-nethernet `ParseDetached` with ES384 only).
pub(crate) fn verify_detached(jws: &str, payload: &[u8], key: &PublicKey) -> Result<()> {
    if jws.len() > MAX_HEADER_TEXT + MAX_SIGNATURE_TEXT + 2 {
        return Err(Error::TooLarge);
    }
    let mut parts = jws.split('.');
    let (Some(header), Some(""), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::Jws("not a detached jws"));
    };
    let mut token = String::with_capacity(jws.len() + payload.len().div_ceil(3) * 4);
    token.push_str(header);
    token.push('.');
    RAW_URL.encode_into(payload, &mut token);
    token.push('.');
    token.push_str(signature);
    let parsed = parse(&token, payload.len())?;
    if parsed.header.alg != Algorithm::Es384 {
        return Err(Error::Jws("algorithm is not ES384"));
    }
    key.verify_es384(parsed.signing_input, &parsed.signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz;

    #[test]
    fn detached_signatures_verify_only_over_their_payload() {
        let key = ServerKey::generate().unwrap();
        let public = key.public_key().unwrap();
        let jws = sign_detached(&key, b"{\"a\":1}").unwrap();
        let (header, rest) = jws.split_once('.').unwrap();
        assert_eq!(RAW_URL.decode(header, 64).unwrap(), DETACHED_HEADER);
        assert!(rest.starts_with('.'));
        verify_detached(&jws, b"{\"a\":1}", &public).unwrap();
        assert!(verify_detached(&jws, b"{\"a\":2}", &public).is_err());
        let other = ServerKey::generate().unwrap().public_key().unwrap();
        assert!(verify_detached(&jws, b"{\"a\":1}", &other).is_err());
        let (_, signature) = rest.split_once('.').unwrap();
        let attached = format!("{header}.{}.{signature}", RAW_URL.encode(b"{\"a\":1}"));
        assert!(matches!(
            verify_detached(&attached, b"{\"a\":1}", &public),
            Err(Error::Jws(_))
        ));
        assert!(verify_detached(&format!("{jws}."), b"{\"a\":1}", &public).is_err());
        assert!(matches!(
            verify_detached(&"A".repeat(4000), b"", &public),
            Err(Error::TooLarge)
        ));
        // A valid ES384 signature under a header naming another algorithm is still rejected.
        for alg in ["none", "HS256", "RS256", "ES256"] {
            let header = format!(r#"{{"alg":"{alg}"}}"#);
            let token = sign_compact(&key, header.as_bytes(), b"{\"a\":1}").unwrap();
            let mut parts = token.split('.');
            let (h, _, s) = (parts.next().unwrap(), parts.next(), parts.next().unwrap());
            let err = verify_detached(&format!("{h}..{s}"), b"{\"a\":1}", &public).unwrap_err();
            assert!(matches!(err, Error::Jws("algorithm is not ES384")), "{alg}");
        }
    }

    #[test]
    fn signed_tokens_parse_and_verify() {
        let key = ServerKey::generate().unwrap();
        let token = sign_es384(&key, &serde_json::json!({"salt": "abc"})).unwrap();
        let jws = parse(&token, 1024).unwrap();
        assert_eq!(jws.header.alg, Algorithm::Es384);
        assert_eq!(jws.header.x5u.as_deref(), Some(key.public_key_base64()));
        assert_eq!(jws.payload, br#"{"salt":"abc"}"#);
        let public = PublicKey::from_base64_spki(key.public_key_base64()).unwrap();
        public
            .verify_es384(jws.signing_input, &jws.signature)
            .unwrap();
        let mut tampered = jws.signing_input.to_vec();
        tampered[0] ^= 1;
        assert!(public.verify_es384(&tampered, &jws.signature).is_err());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let header = RAW_URL.encode(br#"{"alg":"RS256","kid":"k"}"#);
        let ok = format!("{header}.e30.AAAA");
        let jws = parse(&ok, 16).unwrap();
        assert_eq!(jws.header.alg, Algorithm::Rs256);
        assert_eq!(jws.header.kid.as_deref(), Some("k"));
        assert_eq!(jws.signing_input, format!("{header}.e30").as_bytes());
        for bad in [
            String::new(),
            "a.b".to_owned(),
            format!("{ok}.x"),
            format!("{header}.e30=.AAAA"),
            format!("{}.e30.AAAA", RAW_URL.encode(b"{}")),
            format!(
                "{}.e30.AAAA",
                RAW_URL.encode(br#"{"alg":"ES384","crit":["b64"]}"#)
            ),
            format!(
                "{}.e30.AAAA",
                RAW_URL.encode(br#"{"alg":"ES384","alg":"ES384"}"#)
            ),
            format!("{header}.{}.AAAA", "A".repeat(100)),
            format!("{header}.e30.{}", "A".repeat(2000)),
        ] {
            assert!(parse(&bad, 16).is_err(), "{bad}");
        }
        for alg in ["none", "HS256", "PS256", "ES256"] {
            let header = RAW_URL.encode(format!(r#"{{"alg":"{alg}"}}"#).as_bytes());
            let token = format!("{header}.e30.");
            assert_eq!(parse(&token, 16).unwrap().header.alg, Algorithm::Other);
        }
    }

    #[test]
    fn max_text_is_the_longest_token_parse_accepts() {
        const MAX_PAYLOAD: usize = 16 * 1024;
        // A 1,536-byte header JSON is 2,048 base64 characters; a long `kid` fills it.
        let fill = 1536 - br#"{"alg":"ES384","kid":""}"#.len();
        let header_json = format!(r#"{{"alg":"ES384","kid":"{}"}}"#, "k".repeat(fill));
        let header = RAW_URL.encode(header_json.as_bytes());
        let signature = RAW_URL.encode(&[7; 768]);
        assert_eq!(
            (header.len(), signature.len()),
            (MAX_HEADER_TEXT, MAX_SIGNATURE_TEXT)
        );
        let payload = RAW_URL.encode(&vec![b'x'; MAX_PAYLOAD]);
        let longest = format!("{header}.{payload}.{signature}");
        assert_eq!(longest.len(), max_text(MAX_PAYLOAD));
        assert_eq!(
            parse(&longest, MAX_PAYLOAD).unwrap().payload.len(),
            MAX_PAYLOAD
        );
        let payload = RAW_URL.encode(&vec![b'x'; MAX_PAYLOAD + 1]);
        let over = format!("{header}.{payload}.{signature}");
        assert!(matches!(parse(&over, MAX_PAYLOAD), Err(Error::TooLarge)));
    }

    #[test]
    fn mutated_tokens_never_panic() {
        let key = ServerKey::generate().unwrap();
        let token = sign_es384(&key, &serde_json::json!({"salt": "abc"})).unwrap();
        let public = PublicKey::from_base64_spki(key.public_key_base64()).unwrap();
        let ok = fuzz(token.as_bytes(), 4000, |data| {
            let text =
                std::str::from_utf8(data).map_err(|_| crate::protocol::Error::InvalidUtf8)?;
            let jws = parse(text, 1024).map_err(|_| crate::protocol::Error::Size)?;
            public
                .verify_es384(jws.signing_input, &jws.signature)
                .map_err(|_| crate::protocol::Error::Size)
        });
        assert!(ok > 0);
    }
}
