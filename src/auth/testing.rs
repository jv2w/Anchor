//! A test issuer: an RSA key, its OpenID configuration and JWKS, and the tokens it signs.
//!
//! Tests use it in place of the Minecraft authorization service; the engine has no test entry
//! point.

use std::sync::OnceLock;

use aws_lc_rs::rsa::KeyPair;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use serde_json::{Value, json};

use super::Keys;
use super::document::{self, Jwks, OpenId};
use crate::crypto::RAW_URL;

/// The issuer string of the test OpenID configuration, with the trailing slash of the real one.
pub(crate) const ISSUER: &str = "https://issuer.test/";
/// The `kid` of the test key.
pub(crate) const KID: &str = "test-kid";

/// The test issuer every test shares.
static SHARED_ISSUER: OnceLock<Issuer> = OnceLock::new();

/// A test issuer: one RSA-2048 key, generated once per test process.
#[derive(Debug)]
pub(crate) struct Issuer {
    key: KeyPair,
    n: String,
    e: String,
}

impl Issuer {
    /// The shared issuer.
    pub(crate) fn get() -> &'static Self {
        SHARED_ISSUER.get_or_init(Self::generate)
    }

    /// A new issuer with its own key (for "signed by another issuer" cases).
    pub(crate) fn generate() -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "off-tick: test-only key generation"
        )]
        let key = KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let (n, e) = rsa_public_numbers(key.public_key().as_ref());
        let (n, e) = (RAW_URL.encode(n), RAW_URL.encode(e));
        Self { key, n, e }
    }

    /// The verification keys, as the authorization task would publish them.
    pub(crate) fn keys(&self) -> Keys {
        let openid: OpenId =
            serde_json::from_value(Self::openid("https://issuer.test/keys")).unwrap();
        let jwks: Jwks = serde_json::from_value(self.jwks(KID)).unwrap();
        document::keys(&openid, &jwks).unwrap()
    }

    /// The JWKS document with this issuer's key under `kid`.
    pub(crate) fn jwks(&self, kid: &str) -> Value {
        json!({"keys": [
            {"kty": "EC", "use": "sig", "kid": "ec-key", "crv": "P-256"},
            {"kty": "RSA", "use": "sig", "kid": kid, "x5t": "AA", "n": self.n, "e": self.e},
        ]})
    }

    /// Keys for another issuer's JWKS entry, such as one produced by a Go golden.
    pub(crate) fn keys_from(issuer: &str, kid: &str, n: &str, e: &str) -> Keys {
        let openid: OpenId =
            serde_json::from_value(json!({"issuer": issuer, "jwks_uri": "https://x/keys"}))
                .unwrap();
        let jwks: Jwks =
            serde_json::from_value(json!({"keys": [{"kty": "RSA", "kid": kid, "n": n, "e": e}]}))
                .unwrap();
        document::keys(&openid, &jwks).unwrap()
    }

    /// The OpenID configuration document.
    pub(crate) fn openid(jwks_uri: &str) -> Value {
        json!({
            "issuer": ISSUER,
            "jwks_uri": jwks_uri,
            "id_token_signing_alg_values_supported": ["RS256"],
        })
    }

    /// A multiplayer token with the standard header (RS256, the test `kid`).
    pub(crate) fn token(&self, claims: &Value) -> String {
        self.sign(&json!({"alg": "RS256", "kid": KID, "typ": "JWT"}), claims)
    }

    /// The token of `claims` signed with RS256 under `header`, which may name any `alg`.
    pub(crate) fn sign(&self, header: &Value, claims: &Value) -> String {
        self.sign_bytes(header.to_string().as_bytes(), claims.to_string().as_bytes())
    }

    /// The token of arbitrary header and payload bytes signed with RS256.
    pub(crate) fn sign_bytes(&self, header: &[u8], claims: &[u8]) -> String {
        let mut token = RAW_URL.encode(header);
        token.push('.');
        token.push_str(&RAW_URL.encode(claims));
        #[expect(
            clippy::disallowed_types,
            reason = "off-tick: test-only signing; RSA PKCS#1 v1.5 ignores the generator"
        )]
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &rng, token.as_bytes(), &mut signature)
            .unwrap();
        token.push('.');
        token.push_str(&RAW_URL.encode(&signature));
        token
    }
}

/// The big-endian numbers `(n, e)` of a PKCS#1 `RSAPublicKey` (SEQUENCE of INTEGER n, INTEGER e).
fn rsa_public_numbers(der: &[u8]) -> (&[u8], &[u8]) {
    fn element(input: &[u8], tag: u8) -> (&[u8], &[u8]) {
        assert_eq!(input[0], tag);
        let (len, header) = match input[1] {
            short @ 0..=0x7f => (usize::from(short), 2),
            0x81 => (usize::from(input[2]), 3),
            0x82 => (usize::from(u16::from_be_bytes([input[2], input[3]])), 4),
            other => panic!("length form {other:#x}"),
        };
        (&input[header..header + len], &input[header + len..])
    }
    let (body, _) = element(der, 0x30);
    let (n, rest) = element(body, 0x02);
    let (e, _) = element(rest, 0x02);
    (n, e)
}
