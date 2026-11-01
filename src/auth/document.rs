//! The authorization documents (discovery, OpenID configuration, JWKS) and their cache file.
//!
//! The cache holds the last two documents.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Keys;
use crate::crypto::{RsaPublicKey, bounded_vec};

/// The most keys read from a JWKS.
/// Microsoft publishes four.
const MAX_JWKS_KEYS: usize = 64;

/// The contents of `data/auth.json`; unknown keys are ignored.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Cache {
    pub(super) openid: OpenId,
    pub(super) jwks: Jwks,
    /// Unix seconds of the fetch.
    pub(super) fetched_at: i64,
}

/// The OpenID configuration fields Anchor uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct OpenId {
    /// The token issuer, with the trailing slash the service publishes.
    pub(super) issuer: String,
    pub(super) jwks_uri: String,
}

/// A JSON Web Key Set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Jwks {
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_JWKS_KEYS>")]
    pub(super) keys: Vec<Jwk>,
}

/// One JSON Web Key; only RSA signing keys are used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Jwk {
    pub(super) kty: String,
    #[serde(default)]
    pub(super) kid: String,
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub(super) usage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) alg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) n: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) e: Option<String>,
}

/// The discovery response: `result.serviceEnvironments.auth.prod`.
#[derive(Deserialize)]
pub(super) struct Discovery {
    result: DiscoveryResult,
}

impl Discovery {
    /// The issuer of the production authorization environment.
    /// The OpenID configuration is published there.
    pub(super) fn into_issuer(self) -> String {
        self.result.service_environments.auth.prod.issuer
    }
}

#[derive(Deserialize)]
struct DiscoveryResult {
    #[serde(rename = "serviceEnvironments")]
    service_environments: Services,
}

#[derive(Deserialize)]
struct Services {
    auth: Environments,
}

#[derive(Deserialize)]
struct Environments {
    prod: Environment,
}

/// The production authorization environment.
#[derive(Deserialize)]
struct Environment {
    issuer: String,
}

/// The verification keys of a JWKS.
/// They are RSA keys for signing (no `use`, or `sig`) with no `alg` or RS256. Other key types are
/// skipped; a malformed RSA key, a duplicate `kid` or no usable key rejects the whole set.
pub(super) fn keys(openid: &OpenId, jwks: &Jwks) -> Result<Keys, &'static str> {
    if openid.issuer.is_empty() {
        return Err("empty issuer");
    }
    let mut by_kid = BTreeMap::new();
    for jwk in &jwks.keys {
        let signs = jwk.usage.as_deref().is_none_or(|u| u == "sig");
        let rs256 = jwk.alg.as_deref().is_none_or(|a| a == "RS256");
        if jwk.kty != "RSA" || !signs || !rs256 {
            continue;
        }
        let (Some(n), Some(e)) = (&jwk.n, &jwk.e) else {
            return Err("rsa key without n or e");
        };
        if jwk.kid.is_empty() {
            return Err("rsa key without kid");
        }
        let key = RsaPublicKey::from_jwk(n, e).map_err(|_| "invalid rsa key")?;
        if by_kid.insert(jwk.kid.clone(), key).is_some() {
            return Err("duplicate kid");
        }
    }
    if by_kid.is_empty() {
        return Err("no rsa signing key");
    }
    Ok(Keys {
        issuer: openid.issuer.clone(),
        by_kid,
    })
}
