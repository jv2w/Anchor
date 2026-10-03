//! JWS compact serialization (RFC 7515): parsing with size caps, and ES384 signing.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use super::base64::RAW_URL;
use super::{Error, Result, ServerKey};

/// Longest header segment in base64 text. A Bedrock header holds `alg`, `kid`, `typ`, `x5t` or a
/// P-384 `x5u` (160 characters), well under 1 KiB.
const MAX_HEADER_TEXT: usize = 2048;
/// Longest signature segment in base64 text: an RSA-4096 signature is 683 characters.
const MAX_SIGNATURE_TEXT: usize = 1024;

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
    let mut token = RAW_URL.encode(&header);
    token.push('.');
    RAW_URL.encode_into(&payload, &mut token);
    let signature = key.sign_es384(token.as_bytes())?;
    token.push('.');
    RAW_URL.encode_into(&signature, &mut token);
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::PublicKey;
    use crate::protocol::fuzz;

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
