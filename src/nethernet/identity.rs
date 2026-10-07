//! The NetherNet identity: the server's long-lived P-384 key in
//! `data/identity.key`, the `a=identity` assertion of every answer, and verification of the
//! assertion in a client offer.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::sdp;
use crate::crypto::{
    self, Algorithm, PublicKey, STD, ServerKey, parse_jws, sign_detached, sign_jwt_bytes,
    verify_detached,
};
use crate::fs::{self, Access};
use crate::login::{self, MAX_TOKEN_PAYLOAD, Mode, NumericDate, check_times, verify_online_token};

/// Largest key file read. A P-384 PKCS#8 document is under 200 bytes.
const MAX_KEY_FILE: u64 = 4096;
/// Lifetime of the server identity JWT in seconds (BDS: `exp = iat + 60`).
const TOKEN_LIFETIME: i64 = 60;
/// The identity provider domain BDS names for its own key.
const SERVER_DOMAIN: &str = "self";

/// An identity failure: the key file at start, or an offer's assertion (signaling code 37).
#[derive(Debug)]
pub(crate) enum Error {
    /// The key file cannot be read or written.
    Io { path: PathBuf, source: io::Error },
    /// The key file is larger than [`MAX_KEY_FILE`].
    KeyFileTooLarge { path: PathBuf },
    /// The key file is not a P-384 PKCS#8 key.
    KeyFile {
        path: PathBuf,
        source: crypto::Error,
    },
    /// Signing the named part failed.
    Sign(&'static str, crypto::Error),
    /// The named part of an offer's assertion is malformed or does not verify.
    Crypto(&'static str, crypto::Error),
    /// The offer token breaks a login rule. [`login::Error::UnknownKeyId`] asks for one JWKS
    /// refresh, as for the Login token.
    Token(login::Error),
    /// JSON of the named part is malformed.
    Json(&'static str, serde_json::Error),
    /// The named part uses a signature algorithm that is not accepted there.
    Algorithm(&'static str),
    /// A fingerprint has characters outside the hash name and hex digest sets.
    Fingerprint,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                write!(
                    f,
                    "cannot read or write identity key file {}",
                    path.display()
                )
            }
            Self::KeyFileTooLarge { path } => write!(
                f,
                "identity key file {} is larger than {MAX_KEY_FILE} bytes",
                path.display()
            ),
            Self::KeyFile { path, .. } => write!(
                f,
                "identity key file {} is not a P-384 PKCS#8 key",
                path.display()
            ),
            Self::Sign(part, _) => write!(f, "cannot sign the {part}"),
            Self::Crypto(part, _) => write!(f, "{part} is malformed or does not verify"),
            Self::Token(_) => f.write_str("identity token rejected"),
            Self::Json(part, _) => write!(f, "malformed {part} json"),
            Self::Algorithm(part) => write!(f, "{part} signature algorithm not accepted"),
            Self::Fingerprint => f.write_str("fingerprint is not a hash name and a hex digest"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::KeyFile { source, .. } | Self::Sign(_, source) | Self::Crypto(_, source) => {
                Some(source)
            }
            Self::Token(e) => Some(e),
            Self::Json(_, e) => Some(e),
            _ => None,
        }
    }
}

/// An identity result.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// The server's NetherNet identity: a P-384 key kept across restarts, so players confirm a
/// new key only once.
pub(crate) struct ServerIdentity {
    key: ServerKey,
    /// The JWT header as BDS writes it: `{"alg":"ES384","x5u":"<base64 SPKI>"}` and a line feed.
    header: String,
    /// The `cpk` claim: the public key as a P-384 JWK object, keys in BDS order.
    cpk: String,
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl ServerIdentity {
    /// Reads the key from `path` (P-384 PKCS#8 DER, at most [`MAX_KEY_FILE`] bytes), or creates
    /// one there when the file does not exist. A file that cannot be read or parsed is an error
    /// and is left untouched. Blocking: call it on the blocking pool at start.
    pub(crate) fn load_or_create(path: &Path) -> Result<Self> {
        let key = if let Some(der) = read_key_file(path)? {
            ServerKey::from_pkcs8(&der).map_err(|source| Error::KeyFile {
                path: path.to_owned(),
                source,
            })?
        } else {
            let key = ServerKey::generate().map_err(|e| Error::Sign("server key", e))?;
            let der = key.to_pkcs8().map_err(|e| Error::Sign("server key", e))?;
            // On Unix only the owner may read it.
            fs::write_atomic(path, &der, Access::OwnerOnly).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            key
        };
        Self::new(key)
    }

    /// An identity with a new key that no file keeps.
    #[cfg(test)]
    pub(crate) fn generate() -> Result<Self> {
        Self::new(ServerKey::generate().map_err(|e| Error::Sign("server key", e))?)
    }

    fn new(key: ServerKey) -> Result<Self> {
        let (x, y) = key
            .public_key()
            .and_then(|public| public.p384_jwk())
            .map_err(|e| Error::Sign("server key", e))?;
        // Base64 and base64url text needs no JSON escaping.
        let header = format!(
            "{{\"alg\":\"ES384\",\"x5u\":\"{}\"}}\n",
            key.public_key_base64()
        );
        let cpk = format!("{{\"crv\":\"P-384\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}");
        Ok(Self { key, header, cpk })
    }

    /// The `a=identity` value of an answer whose `a=fingerprint` digest is `answer_fingerprint`
    /// (as written there), with the BDS bytes: padded standard base64 of
    /// `{"assertion":"{\"fingerprints\":\"<detached JWS>\",\"token\":\"<JWT>\"}","idp":{"domain":"self","protocol":"default"}}`.
    /// The JWT is issued at `now_unix` and expires 60 seconds later; its header and payload JSON
    /// end in a line feed, as BDS writes them.
    pub(crate) fn assertion(&self, answer_fingerprint: &str, now_unix: i64) -> Result<String> {
        let signed = fingerprint_payload("sha-256", answer_fingerprint)?;
        let exp = now_unix.saturating_add(TOKEN_LIFETIME);
        let claims = format!(
            "{{\"cpk\":{},\"exp\":{exp},\"iat\":{now_unix}}}\n",
            self.cpk
        );
        let token = sign_jwt_bytes(&self.key, self.header.as_bytes(), claims.as_bytes())
            .map_err(|e| Error::Sign("identity token", e))?;
        let fingerprints = sign_detached(&self.key, signed.as_bytes())
            .map_err(|e| Error::Sign("fingerprints", e))?;
        let assertion = serde_json::to_string(&AssertionJson {
            fingerprints: &fingerprints,
            token: &token,
        })
        .map_err(|e| Error::Json("assertion", e))?;
        let json = serde_json::to_vec(&IdentityJson {
            assertion: &assertion,
            idp: IdentityProvider {
                domain: SERVER_DOMAIN,
                protocol: "default",
            },
        })
        .map_err(|e| Error::Json("identity", e))?;
        Ok(STD.encode(&json))
    }
}

#[derive(Serialize)]
struct IdentityJson<'a> {
    assertion: &'a str,
    idp: IdentityProvider<'a>,
}

#[derive(Serialize)]
struct IdentityProvider<'a> {
    domain: &'a str,
    protocol: &'a str,
}

#[derive(Serialize)]
struct AssertionJson<'a> {
    fingerprints: &'a str,
    token: &'a str,
}

/// Verifies an offer's identity assertion against the offer's fingerprint and returns the key it
/// proves (the `cpk` claim), which the Login must use.
///
/// The token follows `mode`: online, every Login `Token` rule; offline, no signature, but the
/// algorithm must be RS256 or ES384 and `exp`, `nbf` and `iat` hold when present. `cpk` is base64
/// SPKI or a P-384 JWK object (BDS writes a JWK, go-nethernet base64). The fingerprint signature
/// is a detached ES384 JWS by `cpk`. Rejecting an online offer that has no assertion is the
/// caller's job.
pub(crate) fn verify_offer(
    assertion: &sdp::Assertion,
    fingerprint: &sdp::Fingerprint,
    mode: Mode<'_>,
    now_unix: i64,
) -> Result<PublicKey> {
    let key = match mode {
        Mode::Online(keys) => {
            verify_online_token(&assertion.token, keys, now_unix)
                .map_err(Error::Token)?
                .1
        }
        Mode::Offline => unverified_token_key(&assertion.token, now_unix)?,
    };
    let signed = fingerprint_payload(&fingerprint.algorithm, &fingerprint.digest)?;
    verify_detached(&assertion.fingerprints, signed.as_bytes(), &key)
        .map_err(|e| Error::Crypto("fingerprints", e))?;
    Ok(key)
}

/// Claims read from an unverified offer token.
#[derive(Deserialize)]
struct OfferClaims {
    cpk: Cpk,
    exp: Option<NumericDate>,
    nbf: Option<NumericDate>,
    iat: Option<NumericDate>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Cpk {
    Spki(String),
    Jwk(Jwk),
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

/// The `cpk` of an offline offer token, read without verifying the token (go-nethernet
/// `claimPublicKey` with `selfSigned` false).
fn unverified_token_key(token: &str, now: i64) -> Result<PublicKey> {
    let jws = parse_jws(token, MAX_TOKEN_PAYLOAD).map_err(|e| Error::Crypto("token", e))?;
    if !matches!(jws.header.alg, Algorithm::Es384 | Algorithm::Rs256) {
        return Err(Error::Algorithm("token"));
    }
    let claims: OfferClaims =
        serde_json::from_slice(&jws.payload).map_err(|e| Error::Json("token", e))?;
    check_times(claims.exp, claims.nbf, claims.iat, now).map_err(Error::Token)?;
    match &claims.cpk {
        Cpk::Spki(text) => PublicKey::from_base64_spki(text),
        Cpk::Jwk(jwk) if jwk.kty == "EC" && jwk.crv == "P-384" => {
            PublicKey::from_p384_jwk(&jwk.x, &jwk.y)
        }
        Cpk::Jwk(_) => Err(crypto::Error::Key),
    }
    .map_err(|e| Error::Crypto("cpk", e))
}

/// The JSON a fingerprint signature covers (go-nethernet `generateFingerprints`, one
/// fingerprint). The values go in unescaped, so only a hash name of letters, digits and `-` and a
/// digest of hex digits and `:` are accepted.
fn fingerprint_payload(algorithm: &str, digest: &str) -> Result<String> {
    let name = !algorithm.is_empty()
        && algorithm
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let hex = !digest.is_empty() && digest.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
    if !name || !hex {
        return Err(Error::Fingerprint);
    }
    Ok(format!(
        "{{\"fingerprint\":[{{\"algorithm\":\"{algorithm}\",\"digest\":\"{digest}\"}}]}}"
    ))
}

/// Reads at most [`MAX_KEY_FILE`] bytes of the key file; `None` when it does not exist.
fn read_key_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read_bounded(path, MAX_KEY_FILE) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Err(Error::KeyFileTooLarge {
            path: path.to_owned(),
        }),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::testing::Issuer;
    use crate::crypto::{RAW_URL, Verifier, verify_jws};
    use crate::nethernet::sdp::tests::{OFFER, identity_line};
    use crate::nethernet::sdp::{Answer, fingerprint_text, parse_offer, write_answer};
    use crate::protocol::{self, fuzz};

    /// Inside both captured tokens' lifetimes: BDS iat 1790993459, exp 1790993519; go-nethernet
    /// exp 1790997059.
    const NOW: i64 = 1_790_993_469;
    /// The `a=identity` value of a captured BDS 1.26.52.3 answer.
    const BDS_IDENTITY: &str = "eyJhc3NlcnRpb24iOiJ7XCJmaW5nZXJwcmludHNcIjpcImV5SmhiR2NpT2lKRlV6TTROQ0o5Li45N0M2d21LYXVlTnlIcVdHcU83ZXZuNVJnU09OZG54NFZLbDFCR0hRUzAwNzZoMm50akF2bXFlRjFiX1k2MXlnU2RpVjR4QnhoWThGcG5lNU5oMnlXRHZZNFBWX0l2UjFseXhtT2hKZW5HU2NzdmNIek5fSGR1VVlDTHZRT29rQ1wiLFwidG9rZW5cIjpcImV5SmhiR2NpT2lKRlV6TTROQ0lzSW5nMWRTSTZJazFJV1hkRlFWbElTMjlhU1hwcU1FTkJVVmxHU3pSRlJVRkRTVVJaWjBGRmVXRXZhbmh3U0VObVRWcGlhazVLUTJwVWJtTmFjREJ3T1c4MFpHZGpUMFphYVZoc2NrRmpOMDlXWjBnMk5rNDJZblpQU2toTGRFRkxVMFl3V1RBNFRqTlBjM3BQVERORFZrRTNRV1pSY3pscllreExSVm8zTkhsRmFHeEtObUZJVDFGc2RUaG9TMGhsUmpGSk9FOXdkMnhSTTNoMVUya3diVnBGUm5JM1Z6QWlmUW8uZXlKamNHc2lPbnNpWTNKMklqb2lVQzB6T0RRaUxDSnJkSGtpT2lKRlF5SXNJbmdpT2lKNVlWOXFlSEJJUTJaTldtSnFUa3BEYWxSdVkxcHdNSEE1YnpSa1oyTlBSbHBwV0d4eVFXTTNUMVpuU0RZMlRqWmlkazlLU0V0MFFVdFRSakJaTURoT0lpd2llU0k2SWpOUGMzcFBURE5EVmtFM1FXWlJjemxyWWt4TFJWbzNOSGxGYUd4S05tRklUMUZzZFRob1MwaGxSakZKT0U5d2QyeFJNM2gxVTJrd2JWcEZSbkkzVnpBaWZTd2laWGh3SWpveE56a3dPVGt6TlRFNUxDSnBZWFFpT2pFM09UQTVPVE0wTlRsOUNnLlBURFlTVVEwWXFfQmFZWS1IZGttM1c5TEU5aXBvTTJZbE1VNktDNUZqRkNmZ1hhbEsyV2k5elFhLUhVNzBqaUZ4WWFLd3MycW5rZ0dUTHQ2ckdVM3lXd0tkU1ZZLWt4ZmdXdDkxM0xYNFhLOHAzRVYwMnBoNzZnV2RyQjFEWkVZXCJ9IiwiaWRwIjp7ImRvbWFpbiI6InNlbGYiLCJwcm90b2NvbCI6ImRlZmF1bHQifX0=";
    /// The `a=fingerprint` digest of the same answer.
    const BDS_FINGERPRINT: &str = "7D:C0:D8:7A:F6:1F:11:CF:20:96:15:45:82:89:DC:2E:E9:12:99:44:CD:60:C7:25:D5:7C:85:7E:19:AE:41:D9";

    /// A per-test directory under the system temp directory, removed on drop.
    struct TempDir(PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-identity-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).unwrap_or(());
            // The server makes the data folder before it loads the key.
            std::fs::create_dir_all(dir.join("data")).unwrap();
            Self(dir)
        }

        fn write(&self, bytes: &[u8]) {
            std::fs::write(self.key(), bytes).unwrap();
        }

        fn read(&self) -> Vec<u8> {
            std::fs::read(self.key()).unwrap()
        }

        fn key(&self) -> PathBuf {
            self.0.join("data").join("identity.key")
        }
    }

    impl Drop for TempDir {
        #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap_or(());
        }
    }

    fn public(identity: &ServerIdentity) -> &str {
        identity.key.public_key_base64()
    }

    #[test]
    fn key_files_are_created_reused_and_left_alone_when_broken() {
        let dir = TempDir::new("files");
        let created = ServerIdentity::load_or_create(&dir.key()).unwrap();
        let bytes = dir.read();
        assert!(bytes.len() < 200, "{}", bytes.len());
        let reused = ServerIdentity::load_or_create(&dir.key()).unwrap();
        assert_eq!(public(&created), public(&reused));
        assert_eq!(dir.read(), bytes);

        let truncated = bytes[..bytes.len() - 1].to_vec();
        for broken in [
            Vec::new(),
            b"not a key".to_vec(),
            truncated,
            vec![0x30; 4097],
        ] {
            dir.write(&broken);
            let err = ServerIdentity::load_or_create(&dir.key()).unwrap_err();
            assert!(
                matches!(err, Error::KeyFile { .. } | Error::KeyFileTooLarge { .. }),
                "{err:?}"
            );
            assert_eq!(dir.read(), broken);
        }
        // Exactly the cap is read (and rejected as a key, not for its size).
        dir.write(&[0x30; 4096]);
        assert!(matches!(
            ServerIdentity::load_or_create(&dir.key()),
            Err(Error::KeyFile { .. })
        ));
        // A folder in place of the file cannot be read.
        let folder = TempDir::new("folder");
        folder.write(b"x");
        let err = ServerIdentity::load_or_create(&folder.0.join("data")).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
    }

    /// Splits a compact JWS into decoded header and payload bytes.
    fn jwt_parts(token: &str) -> (Vec<u8>, Vec<u8>) {
        let mut parts = token.split('.');
        let header = RAW_URL.decode(parts.next().unwrap(), 4096).unwrap();
        let payload = RAW_URL.decode(parts.next().unwrap(), 4096).unwrap();
        (header, payload)
    }

    /// The JSON of an `a=identity` value and its inner `fingerprints` and `token`.
    fn decode(value: &str) -> (String, String, String) {
        let json = String::from_utf8(STD.decode(value, 8192).unwrap()).unwrap();
        let outer: Value = serde_json::from_str(&json).unwrap();
        let inner: Value = serde_json::from_str(outer["assertion"].as_str().unwrap()).unwrap();
        let fingerprints = inner["fingerprints"].as_str().unwrap().to_owned();
        let token = inner["token"].as_str().unwrap().to_owned();
        (json, fingerprints, token)
    }

    /// Text with every long base64 run as `*` and every number as `0`: what stays is the layout.
    fn skeleton(bytes: &[u8]) -> String {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut out = String::new();
        let mut run = String::new();
        let flush = |run: &mut String, out: &mut String| {
            if run.len() >= 16 {
                out.push('*');
            } else if !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit()) {
                out.push('0');
            } else {
                out.push_str(run);
            }
            run.clear();
        };
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || "+/=_-".contains(c) {
                run.push(c);
            } else {
                flush(&mut run, &mut out);
                out.push(c);
            }
        }
        flush(&mut run, &mut out);
        out
    }

    #[test]
    fn the_bds_answer_assertion_verifies_and_shows_its_bytes() {
        let (json, fingerprints, token) = decode(BDS_IDENTITY);
        assert!(json.starts_with(r#"{"assertion":"{\"fingerprints\":\"eyJhbGciOiJFUzM4NCJ9.."#));
        assert!(json.ends_with(r#"\"}","idp":{"domain":"self","protocol":"default"}}"#));
        let (header, payload) = jwt_parts(&token);
        // BDS ends the JWT header and payload JSON with a line feed; the detached header has none.
        assert!(header.starts_with(br#"{"alg":"ES384","x5u":"MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAE"#));
        assert!(header.ends_with(b"\"}\n"));
        assert!(payload.starts_with(br#"{"cpk":{"crv":"P-384","kty":"EC","x":""#));
        assert!(payload.ends_with(b",\"exp\":1790993519,\"iat\":1790993459}\n"));
        let (detached_header, _) = fingerprints.split_once("..").unwrap();
        assert_eq!(
            RAW_URL.decode(detached_header, 64).unwrap(),
            br#"{"alg":"ES384"}"#
        );

        // Self-signed: the JWT verifies with its own x5u key, which is also its cpk.
        let jws = parse_jws(&token, 4096).unwrap();
        let x5u = PublicKey::from_base64_spki(jws.header.x5u.as_deref().unwrap()).unwrap();
        verify_jws(&jws, Verifier::Es384(&x5u)).unwrap();
        let offer = OFFER
            .replace(identity_line(), &format!("a=identity:{BDS_IDENTITY}"))
            .replace(
                OFFER
                    .lines()
                    .find(|l| l.starts_with("a=fingerprint:"))
                    .unwrap(),
                &format!("a=fingerprint:sha-256 {BDS_FINGERPRINT}"),
            );
        let offer = parse_offer(&offer).unwrap();
        let assertion = offer.identity.unwrap();
        assert_eq!(assertion.domain, "self");
        let key = verify_offer(&assertion, &offer.fingerprint, Mode::Offline, NOW).unwrap();
        assert_eq!(key.spki(), x5u.spki());
        // The detached signature covers the digest as written, so another case fails.
        let mut lower = offer.fingerprint.clone();
        lower.digest = lower.digest.to_ascii_lowercase();
        assert!(verify_offer(&assertion, &lower, Mode::Offline, NOW).is_err());
    }

    #[test]
    fn answer_assertions_have_the_bds_layout_and_verify() {
        let dir = TempDir::new("layout");
        let identity = ServerIdentity::load_or_create(&dir.key()).unwrap();
        let fingerprint = fingerprint_text(&[0xab; 32]);
        let value = identity.assertion(&fingerprint, NOW).unwrap();
        let (json, fingerprints, token) = decode(&value);
        assert_eq!(
            json,
            format!(
                r#"{{"assertion":"{{\"fingerprints\":\"{fingerprints}\",\"token\":\"{token}\"}}","idp":{{"domain":"self","protocol":"default"}}}}"#
            )
        );
        let (header, payload) = jwt_parts(&token);
        let key = identity.key.public_key().unwrap();
        let (x, y) = key.p384_jwk().unwrap();
        assert_eq!(
            header,
            format!("{{\"alg\":\"ES384\",\"x5u\":\"{}\"}}\n", public(&identity)).as_bytes()
        );
        assert_eq!(
            payload,
            format!(
                "{{\"cpk\":{{\"crv\":\"P-384\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}},\"exp\":{},\"iat\":{NOW}}}\n",
                NOW + 60
            )
            .as_bytes()
        );
        // Byte for byte the BDS layout, values aside.
        let (bds_json, bds_fingerprints, bds_token) = decode(BDS_IDENTITY);
        assert_eq!(skeleton(json.as_bytes()), skeleton(bds_json.as_bytes()));
        assert_eq!(
            skeleton(fingerprints.as_bytes()),
            skeleton(bds_fingerprints.as_bytes())
        );
        let (bds_header, bds_payload) = jwt_parts(&bds_token);
        assert_eq!(skeleton(&header), skeleton(&bds_header));
        assert_eq!(skeleton(&payload), skeleton(&bds_payload));

        // A peer checks it as go-nethernet does: the JWT with its x5u, then the fingerprints with
        // cpk, after parsing the whole answer.
        let jws = parse_jws(&token, 4096).unwrap();
        verify_jws(&jws, Verifier::Es384(&key)).unwrap();
        let answer = write_answer(&Answer {
            session_id: 7,
            identity: &value,
            addr: "192.0.2.7:19133".parse().unwrap(),
            ice_ufrag: "abcd",
            ice_pwd: "abcdefghijklmnopqrstuv",
            fingerprint: &fingerprint,
        });
        let parsed = parse_offer(&answer.replace("a=setup:active", "a=setup:actpass")).unwrap();
        let proven = verify_offer(
            parsed.identity.as_ref().unwrap(),
            &parsed.fingerprint,
            Mode::Offline,
            NOW,
        )
        .unwrap();
        assert_eq!(proven.spki(), key.spki());
        let late = verify_offer(
            parsed.identity.as_ref().unwrap(),
            &parsed.fingerprint,
            Mode::Offline,
            NOW + 60 + 61,
        );
        assert!(matches!(late, Err(Error::Token(login::Error::Expired))));
        assert!(matches!(
            identity.assertion("AB:\"CD", NOW),
            Err(Error::Fingerprint)
        ));
    }

    #[test]
    fn the_go_nethernet_offer_verifies_offline() {
        let offer = parse_offer(OFFER).unwrap();
        let assertion = offer.identity.unwrap();
        let key = verify_offer(&assertion, &offer.fingerprint, Mode::Offline, NOW).unwrap();
        let (_, payload) = jwt_parts(&assertion.token);
        let claims: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(STD.encode(key.spki()), claims["cpk"].as_str().unwrap());
        let err = verify_offer(
            &assertion,
            &offer.fingerprint,
            Mode::Offline,
            1_790_997_059 + 61,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Token(login::Error::Expired)),
            "{err:?}"
        );
        // Online mode never takes the self-signed token.
        let keys = Issuer::get().keys();
        let err =
            verify_offer(&assertion, &offer.fingerprint, Mode::Online(&keys), NOW).unwrap_err();
        assert!(
            matches!(err, Error::Token(login::Error::Algorithm("token"))),
            "{err:?}"
        );
    }

    fn player() -> &'static ServerKey {
        static PLAYER: std::sync::OnceLock<ServerKey> = std::sync::OnceLock::new();
        PLAYER.get_or_init(|| ServerKey::generate().unwrap())
    }

    fn fingerprint() -> sdp::Fingerprint {
        let bytes = [0x5a; 32];
        sdp::Fingerprint {
            algorithm: "sha-256".to_owned(),
            digest: fingerprint_text(&bytes).to_ascii_lowercase(),
            bytes,
        }
    }

    fn online_claims() -> Value {
        json!({
            "iss": crate::auth::testing::ISSUER,
            "aud": "api://auth-minecraft-services/multiplayer",
            "exp": NOW + 3600,
            "iat": NOW - 60,
            "cpk": player().public_key_base64(),
            "xid": "2535412345678901",
            "xname": "Steve",
        })
    }

    /// An assertion with `token` and a detached signature by `key` over `fingerprint`.
    fn assertion(token: String, key: &ServerKey, fingerprint: &sdp::Fingerprint) -> sdp::Assertion {
        let signed = fingerprint_payload(&fingerprint.algorithm, &fingerprint.digest).unwrap();
        sdp::Assertion {
            token,
            fingerprints: sign_detached(key, signed.as_bytes()).unwrap(),
            domain: "https://authorization.franchise.minecraft-services.net".to_owned(),
        }
    }

    /// A detached JWS by `key` over `fingerprint` under `header`, whatever algorithm it names.
    fn detached_with_header(
        header: &str,
        key: &ServerKey,
        fingerprint: &sdp::Fingerprint,
    ) -> String {
        let signed = fingerprint_payload(&fingerprint.algorithm, &fingerprint.digest).unwrap();
        let token = sign_jwt_bytes(key, header.as_bytes(), signed.as_bytes()).unwrap();
        let mut parts = token.split('.');
        let (h, _, s) = (parts.next().unwrap(), parts.next(), parts.next().unwrap());
        format!("{h}..{s}")
    }

    /// A self-signed offline token with these claims, as go-nethernet writes it.
    fn offline_token(claims: &Value) -> String {
        sign_jwt_bytes(
            player(),
            br#"{"alg":"ES384"}"#,
            claims.to_string().as_bytes(),
        )
        .unwrap()
    }

    type Check = fn(&Error) -> bool;

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one table lists every rejected case"
    )]
    fn online_offers_need_a_valid_multiplayer_token_and_signature() {
        let issuer = Issuer::get();
        let keys = issuer.keys();
        let fp = fingerprint();
        let good = assertion(issuer.token(&online_claims()), player(), &fp);
        let key = verify_offer(&good, &fp, Mode::Online(&keys), NOW).unwrap();
        assert_eq!(STD.encode(key.spki()), player().public_key_base64());

        let other = ServerKey::generate().unwrap();
        let mut other_fp = fingerprint();
        other_fp.digest = other_fp.digest.replacen("5a", "5b", 1);
        let mut bad_signature = good.clone();
        let (head, signature) = bad_signature.fingerprints.split_once("..").unwrap();
        let mut sig = RAW_URL.decode(signature, 128).unwrap();
        sig[5] ^= 1;
        bad_signature.fingerprints = format!("{head}..{}", RAW_URL.encode(&sig));
        let mut expired_claims = online_claims();
        expired_claims["exp"] = json!(NOW - 61);
        let with_fingerprints = |fingerprints: String| sdp::Assertion {
            fingerprints,
            ..good.clone()
        };
        let cases: Vec<(&str, sdp::Assertion, &sdp::Fingerprint, Check)> = vec![
            ("bad fingerprint signature", bad_signature, &fp, |e| {
                matches!(e, Error::Crypto("fingerprints", crypto::Error::Signature))
            }),
            (
                "signed by another key",
                assertion(issuer.token(&online_claims()), &other, &fp),
                &fp,
                |e| matches!(e, Error::Crypto("fingerprints", _)),
            ),
            ("digest mismatch", good.clone(), &other_fp, |e| {
                matches!(e, Error::Crypto("fingerprints", _))
            }),
            (
                "detached alg none",
                with_fingerprints(detached_with_header(r#"{"alg":"none"}"#, player(), &fp)),
                &fp,
                |e| matches!(e, Error::Crypto("fingerprints", crypto::Error::Jws(_))),
            ),
            (
                "detached alg HS256",
                with_fingerprints(detached_with_header(r#"{"alg":"HS256"}"#, player(), &fp)),
                &fp,
                |e| matches!(e, Error::Crypto("fingerprints", crypto::Error::Jws(_))),
            ),
            (
                "detached alg RS256",
                with_fingerprints(detached_with_header(r#"{"alg":"RS256"}"#, player(), &fp)),
                &fp,
                |e| matches!(e, Error::Crypto("fingerprints", crypto::Error::Jws(_))),
            ),
            (
                "attached payload",
                with_fingerprints(
                    sign_jwt_bytes(
                        player(),
                        br#"{"alg":"ES384"}"#,
                        fingerprint_payload("sha-256", &fp.digest)
                            .unwrap()
                            .as_bytes(),
                    )
                    .unwrap(),
                ),
                &fp,
                |e| matches!(e, Error::Crypto("fingerprints", crypto::Error::Jws(_))),
            ),
            (
                "expired token",
                assertion(issuer.token(&expired_claims), player(), &fp),
                &fp,
                |e| matches!(e, Error::Token(login::Error::Expired)),
            ),
            (
                "unknown kid",
                assertion(
                    issuer.sign(&json!({"alg": "RS256", "kid": "nope"}), &online_claims()),
                    player(),
                    &fp,
                ),
                &fp,
                |e| matches!(e, Error::Token(login::Error::UnknownKeyId)),
            ),
            (
                "other issuer",
                assertion(Issuer::generate().token(&online_claims()), player(), &fp),
                &fp,
                |e| matches!(e, Error::Token(login::Error::Signature("token"))),
            ),
            (
                "malformed cpk",
                assertion(
                    issuer.token(&{
                        let mut c = online_claims();
                        c["cpk"] = json!("AAAA");
                        c
                    }),
                    player(),
                    &fp,
                ),
                &fp,
                |e| matches!(e, Error::Token(login::Error::Crypto("client key", _))),
            ),
        ];
        for (name, assertion, fingerprint, expected) in cases {
            let err = verify_offer(&assertion, fingerprint, Mode::Online(&keys), NOW).unwrap_err();
            assert!(expected(&err), "{name}: {err:?}");
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one table lists every rejected case"
    )]
    fn offline_offers_need_a_well_formed_token_and_signature() {
        let fp = fingerprint();
        let (x, y) = player().public_key().unwrap().p384_jwk().unwrap();
        let claims = |cpk: Value| json!({"cpk": cpk, "exp": NOW + 60, "iat": NOW});
        for cpk in [
            json!(player().public_key_base64()),
            json!({"crv": "P-384", "kty": "EC", "x": x, "y": y}),
            json!({"kty": "EC", "crv": "P-384", "x": x, "y": y, "kid": "extra"}),
        ] {
            let offer = assertion(offline_token(&claims(cpk)), player(), &fp);
            let key = verify_offer(&offer, &fp, Mode::Offline, NOW).unwrap();
            assert_eq!(STD.encode(key.spki()), player().public_key_base64());
        }
        // Without times, and with an RS256 header that is not checked offline.
        let no_times = json!({"cpk": player().public_key_base64()});
        verify_offer(
            &assertion(offline_token(&no_times), player(), &fp),
            &fp,
            Mode::Offline,
            NOW,
        )
        .unwrap();
        let rs256 = Issuer::generate().token(&no_times);
        verify_offer(&assertion(rs256, player(), &fp), &fp, Mode::Offline, NOW).unwrap();

        let short = RAW_URL.encode(&[1; 47]);
        let unsigned = |header: &str, claims: &Value| {
            format!(
                "{}.{}.c2ln",
                RAW_URL.encode(header.as_bytes()),
                RAW_URL.encode(claims.to_string().as_bytes())
            )
        };
        let good_claims = claims(json!(player().public_key_base64()));
        let cases: Vec<(&str, String, Check)> = vec![
            (
                "expired",
                offline_token(&json!({"cpk": player().public_key_base64(), "exp": NOW - 61})),
                |e| matches!(e, Error::Token(login::Error::Expired)),
            ),
            (
                "future nbf",
                offline_token(&json!({"cpk": player().public_key_base64(), "nbf": NOW + 61})),
                |e| matches!(e, Error::Token(login::Error::NotYetValid)),
            ),
            ("no cpk", offline_token(&json!({"exp": NOW + 60})), |e| {
                matches!(e, Error::Json("token", _))
            }),
            (
                "cpk not base64",
                offline_token(&claims(json!("AAAA"))),
                |e| matches!(e, Error::Crypto("cpk", _)),
            ),
            ("cpk number", offline_token(&claims(json!(5))), |e| {
                matches!(e, Error::Json("token", _))
            }),
            (
                "jwk kty RSA",
                offline_token(&claims(
                    json!({"kty": "RSA", "crv": "P-384", "x": x, "y": y}),
                )),
                |e| matches!(e, Error::Crypto("cpk", crypto::Error::Key)),
            ),
            (
                "jwk crv P-256",
                offline_token(&claims(
                    json!({"kty": "EC", "crv": "P-256", "x": x, "y": y}),
                )),
                |e| matches!(e, Error::Crypto("cpk", crypto::Error::Key)),
            ),
            (
                "jwk short x",
                offline_token(&claims(
                    json!({"kty": "EC", "crv": "P-384", "x": short, "y": y}),
                )),
                |e| matches!(e, Error::Crypto("cpk", crypto::Error::Key)),
            ),
            (
                "jwk without y",
                offline_token(&claims(json!({"kty": "EC", "crv": "P-384", "x": x}))),
                |e| matches!(e, Error::Json("token", _)),
            ),
            (
                "token alg none",
                unsigned(r#"{"alg":"none"}"#, &good_claims),
                |e| matches!(e, Error::Algorithm("token")),
            ),
            (
                "token alg HS256",
                unsigned(r#"{"alg":"HS256"}"#, &good_claims),
                |e| matches!(e, Error::Algorithm("token")),
            ),
            ("token not a jws", "a.b".to_owned(), |e| {
                matches!(e, Error::Crypto("token", _))
            }),
        ];
        for (name, token, expected) in cases {
            let err = verify_offer(&assertion(token, player(), &fp), &fp, Mode::Offline, NOW)
                .unwrap_err();
            assert!(expected(&err), "{name}: {err:?}");
        }
        // The key that signs the fingerprints must be the token's cpk.
        let other = ServerKey::generate().unwrap();
        let offer = assertion(offline_token(&good_claims), &other, &fp);
        assert!(matches!(
            verify_offer(&offer, &fp, Mode::Offline, NOW),
            Err(Error::Crypto("fingerprints", _))
        ));
    }

    #[test]
    fn mutated_offline_tokens_never_panic() {
        let fp = fingerprint();
        let (x, y) = player().public_key().unwrap().p384_jwk().unwrap();
        let claims =
            json!({"cpk": {"crv": "P-384", "kty": "EC", "x": x, "y": y}, "exp": NOW + 60, "iat": NOW})
                .to_string();
        let signed = fingerprint_payload(&fp.algorithm, &fp.digest).unwrap();
        let fingerprints = sign_detached(player(), signed.as_bytes()).unwrap();
        let passed = fuzz(claims.as_bytes(), 3000, |mutated| {
            let token = format!(
                "{}.{}.c2ln",
                RAW_URL.encode(br#"{"alg":"ES384"}"#),
                RAW_URL.encode(mutated)
            );
            let offer = sdp::Assertion {
                token,
                fingerprints: fingerprints.clone(),
                domain: "self".to_owned(),
            };
            verify_offer(&offer, &fp, Mode::Offline, NOW)
                .map(|_| ())
                .map_err(|_| protocol::Error::Size)
        });
        assert!(passed > 0);
    }
}
