//! Cryptography over aws-lc-rs: ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, randomness, base64, JWS and RakNet encryption.

mod base64;
pub(crate) mod encryption;
mod json;
mod jws;
mod md5;

use std::fmt;

use aws_lc_rs::agreement::{self, ECDH_P384};
use aws_lc_rs::digest;
use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::signature::{
    ECDSA_P384_SHA384_FIXED, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
    ParsedPublicKey, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
};

pub(crate) use base64::{RAW_STD, RAW_URL, STD};
pub(crate) use json::{bounded_vec, is_object_or_null};
pub(crate) use jws::{
    Algorithm, Jws, parse as parse_jws, sign_compact as sign_jwt_bytes, sign_detached,
    sign_es384 as sign_jwt, verify_detached,
};
pub(crate) use md5::md5;

/// Length of an ES384 signature: `r || s`, 48 bytes each.
const ES384_SIGNATURE_LEN: usize = 96;
/// Longest base64 SPKI accepted for a P-384 key. The DER is 120 bytes (160 characters); the cap
/// leaves room for other valid encodings while bounding the allocation.
const MAX_SPKI_TEXT: usize = 512;
/// RSA modulus sizes accepted for the multiplayer token: 2048 to 4096 bits. Microsoft signs with
/// 2048-bit keys; larger moduli only cost verification time.
const RSA_MODULUS_BITS: std::ops::RangeInclusive<usize> = 2048..=4096;
/// The DER of a P-384 `SubjectPublicKeyInfo` up to its uncompressed point: SEQUENCE {
/// SEQUENCE { id-ecPublicKey, secp384r1 }, BIT STRING with no unused bits }.
const P384_SPKI_PREFIX: [u8; 23] = [
    0x30, 0x76, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b,
    0x81, 0x04, 0x00, 0x22, 0x03, 0x62, 0x00,
];
/// Bytes in one P-384 coordinate.
const P384_COORDINATE_LEN: usize = 48;

/// A cryptographic failure. Every variant means the input is not trusted.
#[derive(Debug)]
pub(crate) enum Error {
    /// Base64 text is malformed.
    Base64,
    /// Input is longer than the cap for its kind.
    TooLarge,
    /// A JWS is malformed.
    Jws(&'static str),
    /// JSON is malformed or has the wrong shape.
    Json(serde_json::Error),
    /// A key is malformed or not of the expected kind.
    Key,
    /// A signature does not verify.
    Signature,
    /// An encrypted packet's checksum does not match.
    Checksum,
    /// aws-lc failed internally (randomness, key generation, cipher or key agreement).
    Internal,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base64 => f.write_str("malformed base64"),
            Self::TooLarge => f.write_str("input too large"),
            Self::Jws(reason) => write!(f, "malformed jws: {reason}"),
            Self::Json(_) => f.write_str("malformed json"),
            Self::Key => f.write_str("malformed or unexpected key"),
            Self::Signature => f.write_str("signature does not verify"),
            Self::Checksum => f.write_str("packet checksum mismatch"),
            Self::Internal => f.write_str("internal crypto failure"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

/// A crypto result.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Fills `out` with cryptographically secure random bytes. The only randomness source in Anchor
/// outside the engine's seeded generator.
pub(crate) fn fill_random(out: &mut [u8]) -> Result<()> {
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the crypto module is the one place that draws OS randomness"
    )]
    aws_lc_rs::rand::fill(out).map_err(|_| Error::Internal)
}

/// SHA-256 of the concatenation of `parts`.
pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    for part in parts {
        context.update(part);
    }
    let mut out = [0u8; 32];
    for (o, d) in out.iter_mut().zip(context.finish().as_ref()) {
        *o = *d;
    }
    out
}

/// The server's P-384 key: it signs the handshake JWT (ES384) and agrees on the RakNet
/// encryption key (ECDH) with the same scalar, as gophertunnel does.
///
/// aws-lc-rs keeps signing and agreement keys apart, so the key pair is exported as PKCS#8 once
/// and imported again as an agreement key. [`ServerKey::from_pkcs8`] checks that both halves have
/// the same public key.
pub(crate) struct ServerKey {
    signing: EcdsaKeyPair,
    agreement: agreement::PrivateKey,
    /// The SPKI DER in standard base64: the JWT `x5u` header.
    public: String,
}

impl fmt::Debug for ServerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl ServerKey {
    /// A new random key.
    pub(crate) fn generate() -> Result<Self> {
        #[expect(
            clippy::disallowed_methods,
            reason = "off-tick: the crypto module is the one place that draws OS randomness"
        )]
        let pair = EcdsaKeyPair::generate(&ECDSA_P384_SHA384_FIXED_SIGNING)
            .map_err(|_| Error::Internal)?;
        let pkcs8 = pair.to_pkcs8v1().map_err(|_| Error::Internal)?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// The key in a PKCS#8 v1 document (a P-384 EC private key).
    pub(crate) fn from_pkcs8(der: &[u8]) -> Result<Self> {
        let signing = EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_FIXED_SIGNING, der)
            .map_err(|_| Error::Key)?;
        let agreement =
            agreement::PrivateKey::from_private_key_der(&ECDH_P384, der).map_err(|_| Error::Key)?;
        let spki = signing.public_key().as_der().map_err(|_| Error::Key)?;
        let agreement_spki = agreement
            .compute_public_key()
            .and_then(|p| p.as_der())
            .map_err(|_| Error::Key)?;
        if spki.as_ref() != agreement_spki.as_ref() {
            return Err(Error::Key);
        }
        Ok(Self {
            signing,
            agreement,
            public: STD.encode(spki.as_ref()),
        })
    }

    /// The key in a PKCS#8 v1 document, the form [`ServerKey::from_pkcs8`] reads.
    pub(crate) fn to_pkcs8(&self) -> Result<Vec<u8>> {
        let document = self.signing.to_pkcs8v1().map_err(|_| Error::Internal)?;
        Ok(document.as_ref().to_vec())
    }

    /// The public key as base64 SPKI DER.
    pub(crate) fn public_key_base64(&self) -> &str {
        &self.public
    }

    /// The public key, for verifying this key's own signatures.
    pub(crate) fn public_key(&self) -> Result<PublicKey> {
        PublicKey::from_base64_spki(&self.public)
    }

    /// An ES384 signature of `message` (randomized nonce).
    pub(crate) fn sign_es384(&self, message: &[u8]) -> Result<[u8; ES384_SIGNATURE_LEN]> {
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: the crypto module is the one place that draws OS randomness"
        )]
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let signature = self
            .signing
            .sign(&rng, message)
            .map_err(|_| Error::Internal)?;
        <[u8; ES384_SIGNATURE_LEN]>::try_from(signature.as_ref()).map_err(|_| Error::Internal)
    }

    /// The ECDH P-384 shared secret (the x coordinate) with `peer`.
    pub(crate) fn agree(&self, peer: &PublicKey) -> Result<[u8; 48]> {
        let peer = agreement::UnparsedPublicKey::new(&ECDH_P384, peer.spki());
        agreement::agree(&self.agreement, peer, Error::Key, |secret| {
            <[u8; 48]>::try_from(secret).map_err(|_| Error::Internal)
        })
    }
}

/// A client's P-384 public key, parsed once from base64 SPKI DER: it verifies ES384 signatures
/// and is the peer in the encryption handshake.
#[derive(Debug)]
pub(crate) struct PublicKey(ParsedPublicKey);

impl PublicKey {
    /// Parses standard base64 of an X.509 `SubjectPublicKeyInfo` holding a P-384 point. A raw
    /// SEC1 point is rejected (gophertunnel `ParsePublicKey` takes only PKIX).
    pub(crate) fn from_base64_spki(text: &str) -> Result<Self> {
        if text.len() > MAX_SPKI_TEXT {
            return Err(Error::TooLarge);
        }
        let der = STD.decode(text, MAX_SPKI_TEXT)?;
        // aws-lc also takes a bare SEC1 point and ignores bytes after the SPKI; Go's
        // `x509.ParsePKIXPublicKey` rejects both.
        if !is_one_der_sequence(&der) {
            return Err(Error::Key);
        }
        ParsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, der)
            .map(Self)
            .map_err(|_| Error::Key)
    }

    /// Verifies an ES384 signature (`r || s`) of `message`.
    pub(crate) fn verify_es384(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        if signature.len() != ES384_SIGNATURE_LEN {
            return Err(Error::Signature);
        }
        self.0
            .verify_sig(message, signature)
            .map_err(|_| Error::Signature)
    }

    /// Parses a P-384 JWK's coordinates (RFC 7518 6.2.1): `x` and `y` in base64url without
    /// padding, 48 bytes each. A point off the curve is rejected.
    pub(crate) fn from_p384_jwk(x: &str, y: &str) -> Result<Self> {
        let x = RAW_URL.decode(x, P384_COORDINATE_LEN)?;
        let y = RAW_URL.decode(y, P384_COORDINATE_LEN)?;
        if x.len() != P384_COORDINATE_LEN || y.len() != P384_COORDINATE_LEN {
            return Err(Error::Key);
        }
        let mut der = Vec::with_capacity(P384_SPKI_PREFIX.len() + 1 + 2 * P384_COORDINATE_LEN);
        der.extend_from_slice(&P384_SPKI_PREFIX);
        der.push(0x04);
        der.extend_from_slice(&x);
        der.extend_from_slice(&y);
        ParsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, der)
            .map(Self)
            .map_err(|_| Error::Key)
    }

    /// The JWK coordinates `(x, y)` in base64url without padding.
    pub(crate) fn p384_jwk(&self) -> Result<(String, String)> {
        let (x, y) = self
            .spki()
            .strip_prefix(&P384_SPKI_PREFIX[..])
            .and_then(|point| point.strip_prefix(&[0x04][..]))
            .filter(|xy| xy.len() == 2 * P384_COORDINATE_LEN)
            .and_then(|xy| xy.split_at_checked(P384_COORDINATE_LEN))
            .ok_or(Error::Key)?;
        Ok((RAW_URL.encode(x), RAW_URL.encode(y)))
    }

    /// The SPKI DER bytes.
    pub(crate) fn spki(&self) -> &[u8] {
        self.0.as_ref()
    }
}

/// An RSA public key from a JWK, for RS256 (the multiplayer token).
#[derive(Debug)]
pub(crate) struct RsaPublicKey(ParsedPublicKey);

impl RsaPublicKey {
    /// The key with modulus `n` and exponent `e`, both base64url without padding (RFC 7518).
    /// The modulus must be 2048 to 4096 bits and the exponent odd, at least 3 and at most 32 bits.
    pub(crate) fn from_jwk(n: &str, e: &str) -> Result<Self> {
        let n = RAW_URL.decode(n, *RSA_MODULUS_BITS.end() / 8 + 1)?;
        let e = RAW_URL.decode(e, 8)?;
        let n = trim_leading_zeros(&n);
        let e = trim_leading_zeros(&e);
        let bits = n.first().map_or(0, |&top| {
            n.len() * 8 - usize::try_from(top.leading_zeros()).unwrap_or(8)
        });
        let odd = e.last().is_some_and(|b| b & 1 == 1);
        if !RSA_MODULUS_BITS.contains(&bits) || e.len() > 4 || !odd || e == [1] {
            return Err(Error::Key);
        }
        RsaPublicKeyComponents { n, e }
            .to_parsed_public_key(&RSA_PKCS1_2048_8192_SHA256)
            .map(Self)
            .map_err(|_| Error::Key)
    }

    /// Verifies an RS256 signature of `message`.
    pub(crate) fn verify_rs256(&self, message: &[u8], sig: &[u8]) -> Result<()> {
        self.0
            .verify_sig(message, sig)
            .map_err(|_| Error::Signature)
    }
}

/// Whether `der` is exactly one DER SEQUENCE with a definite length.
fn is_one_der_sequence(der: &[u8]) -> bool {
    let (header, len) = match *der {
        [0x30, len @ 0..=0x7f, ..] => (2, usize::from(len)),
        [0x30, 0x81, len @ 0x80..=0xff, ..] => (3, usize::from(len)),
        [0x30, 0x82, hi @ 1..=0xff, lo, ..] => (4, usize::from(u16::from_be_bytes([hi, lo]))),
        _ => return false,
    };
    header + len == der.len()
}

fn trim_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

/// Verifies RS256 or ES384 over a parsed JWS; any other algorithm fails.
pub(crate) fn verify_jws(jws: &Jws<'_>, key: Verifier<'_>) -> Result<()> {
    match (jws.header.alg, key) {
        (Algorithm::Es384, Verifier::Es384(key)) => {
            key.verify_es384(jws.signing_input, &jws.signature)
        }
        (Algorithm::Rs256, Verifier::Rs256(key)) => {
            key.verify_rs256(jws.signing_input, &jws.signature)
        }
        _ => Err(Error::Signature),
    }
}

/// The key a JWS must verify with, which also fixes the accepted algorithm.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Verifier<'a> {
    Es384(&'a PublicKey),
    Rs256(&'a RsaPublicKey),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_fips_180_vectors() {
        let hex = |b: [u8; 32]| {
            use std::fmt::Write as _;
            b.iter().fold(String::new(), |mut s, b| {
                write!(s, "{b:02x}").unwrap();
                s
            })
        };
        assert_eq!(
            hex(sha256(&[b"abc"])),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256(&[b"a", b"", b"bc"]), sha256(&[b"abc"]));
        assert_eq!(
            hex(sha256(&[])),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn random_bytes_differ() {
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        fill_random(&mut a).unwrap();
        fill_random(&mut b).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn one_server_key_signs_and_agrees() {
        // PLAN.md 9.2 "검증 필요(M2)": the same P-384 scalar signs a JWT (ES384) and does ECDH.
        let server = ServerKey::generate().unwrap();
        let client = ServerKey::generate().unwrap();
        let token = sign_jwt(&server, &serde_json::json!({"salt": "x"})).unwrap();
        let jws = parse_jws(&token, 64).unwrap();
        let x5u = jws.header.x5u.as_deref().unwrap();
        let server_public = PublicKey::from_base64_spki(x5u).unwrap();
        verify_jws(&jws, Verifier::Es384(&server_public)).unwrap();
        let client_public = PublicKey::from_base64_spki(client.public_key_base64()).unwrap();
        let ours = server.agree(&client_public).unwrap();
        let theirs = client.agree(&server_public).unwrap();
        assert_eq!(ours, theirs);
        assert_ne!(ours, [0; 48]);
    }

    #[test]
    fn pkcs8_keys_round_trip_and_garbage_is_rejected() {
        let a = ServerKey::generate().unwrap();
        let pkcs8 = a.to_pkcs8().unwrap();
        let b = ServerKey::from_pkcs8(&pkcs8).unwrap();
        assert_eq!(a.public_key_base64(), b.public_key_base64());
        assert!(matches!(
            ServerKey::from_pkcs8(b"\x30\x00"),
            Err(Error::Key)
        ));
    }

    #[test]
    fn p384_jwks_round_trip_and_bad_coordinates_are_rejected() {
        let key = ServerKey::generate().unwrap();
        let public = key.public_key().unwrap();
        let (x, y) = public.p384_jwk().unwrap();
        assert_eq!((x.len(), y.len()), (64, 64));
        let back = PublicKey::from_p384_jwk(&x, &y).unwrap();
        assert_eq!(back.spki(), public.spki());
        let sig = key.sign_es384(b"m").unwrap();
        back.verify_es384(b"m", &sig).unwrap();
        // The coordinates of the BDS 1.26.52 identity key in its answer (PLAN.md 10.3) match the
        // SPKI in the same token's `x5u`.
        let x5u = "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEya/jxpHCfMZbjNJCjTncZp0p9o4dgcOFZiXlrAc7OVgH66N6bvOJHKtAKSF0Y08N3OszOL3CVA7AfQs9kbLKEZ74yEhlJ6aHOQlu8hKHeF1I8OpwlQ3xuSi0mZEFr7W0";
        let bds = PublicKey::from_p384_jwk(
            "ya_jxpHCfMZbjNJCjTncZp0p9o4dgcOFZiXlrAc7OVgH66N6bvOJHKtAKSF0Y08N",
            "3OszOL3CVA7AfQs9kbLKEZ74yEhlJ6aHOQlu8hKHeF1I8OpwlQ3xuSi0mZEFr7W0",
        )
        .unwrap();
        assert_eq!(STD.encode(bds.spki()), x5u);
        let short = RAW_URL.encode(&[1; 47]);
        let long = RAW_URL.encode(&[1; 49]);
        let mut off_curve = RAW_URL.decode(&y, 48).unwrap();
        off_curve[47] ^= 1;
        for (x, y) in [
            (short.as_str(), y.as_str()),
            (x.as_str(), long.as_str()),
            (x.as_str(), &RAW_URL.encode(&off_curve)),
            (&format!("{x}="), y.as_str()),
            (
                &format!("+{}", x.chars().skip(1).collect::<String>()),
                y.as_str(),
            ),
            ("", ""),
        ] {
            assert!(PublicKey::from_p384_jwk(x, y).is_err(), "{x} {y}");
        }
    }

    #[test]
    fn rsa_jwks_need_a_sane_modulus_and_exponent() {
        // A key from the live Minecraft authorization JWKS (kid 472E53B5..., 2026-10-03).
        let n = "-nr2A74AK1-AqrGPw5VCFpznoWDIH9AGinoFRsIdakvsauqblvC30ZkzUi6p1_4009aV_Wn1e4M-V98-Rpj1SEyK1EiQoj5DRMmye3an-3NfHKB8SoXl1TvQSYCYBkLN-TTz27aHYsL3eR3xFRvcmJ0Pr37BFlkf0x3aWp3zuH3-qTY08M5M39IY1mBR0k3DVB7MAeJ6dEcjeVclI3-Fc9-qr6qtG9m8PssWt_Q0jKr2RXPhvAPjd7UOYjjWVySR6sL7IoPUDO5ITElUI9XPpNCrUaP2Ig5ytfAgYqdAMAZsScDdSnJXuQ5gd2p8TkTGB3Y5vD9upnfrtQP8d7PmjQ";
        let key = RsaPublicKey::from_jwk(n, "AQAB").unwrap();
        assert!(key.verify_rs256(b"m", &[0; 256]).is_err());
        let modulus = RAW_URL.decode(n, 512).unwrap();
        let mut padded = vec![0];
        padded.extend_from_slice(&modulus);
        assert!(RsaPublicKey::from_jwk(&RAW_URL.encode(&padded), "AQAB").is_ok());
        for (n, e) in [
            (RAW_URL.encode(&[0xff; 128]), "AQAB".to_owned()),
            (RAW_URL.encode(&[0xff; 513]), "AQAB".to_owned()),
            (RAW_URL.encode(&[0x7f; 256]), "AQAB".to_owned()),
            (n.to_owned(), RAW_URL.encode(&[1])),
            (n.to_owned(), RAW_URL.encode(&[2])),
            (n.to_owned(), RAW_URL.encode(&[1, 0, 0, 0, 1])),
            (n.to_owned(), String::new()),
            (n.to_owned(), "AQAB=".to_owned()),
        ] {
            assert!(RsaPublicKey::from_jwk(&n, &e).is_err(), "{n} {e}");
        }
    }

    #[test]
    fn client_keys_must_be_p384_spki() {
        let key = ServerKey::generate().unwrap();
        let spki = STD.decode(key.public_key_base64(), 512).unwrap();
        assert!(PublicKey::from_base64_spki(key.public_key_base64()).is_ok());
        // The bare SEC1 point at the end of the SPKI is not accepted.
        let point = &spki[spki.len() - 97..];
        assert!(matches!(
            PublicKey::from_base64_spki(&STD.encode(point)),
            Err(Error::Key)
        ));
        let mut trailing = spki.clone();
        trailing.push(0);
        assert!(PublicKey::from_base64_spki(&STD.encode(&trailing)).is_err());
        let mut off_curve = spki.clone();
        *off_curve.last_mut().unwrap() ^= 1;
        assert!(PublicKey::from_base64_spki(&STD.encode(&off_curve)).is_err());
        // A P-256 key (from the Go standard library's test data) is the wrong curve.
        let p256 = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEQiBIUwHqzdOfK1C8U+8lHzCC5rbVp+rjwK3dP3nOxMjDA1Q5c3hu2KcsPFm9CnCUZuSyhDKH7nrSvx3V4l4hRQ==";
        assert!(PublicKey::from_base64_spki(p256).is_err());
        assert!(matches!(
            PublicKey::from_base64_spki(&"A".repeat(600)),
            Err(Error::TooLarge)
        ));
        let sig = key.sign_es384(b"m").unwrap();
        let public = PublicKey::from_base64_spki(key.public_key_base64()).unwrap();
        public.verify_es384(b"m", &sig).unwrap();
        assert!(public.verify_es384(b"m", &sig[..95]).is_err());
        assert!(public.verify_es384(b"n", &sig).is_err());
    }
}
