//! Blocking HTTPS fetches of the authorization documents through ureq.
//!
//! ureq uses rustls with the aws-lc-rs provider and the webpki roots. Run only on the blocking
//! pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout, time};

use super::document::{Discovery, Jwks, OpenId};
use super::{Error, MAX_DOCUMENT_BYTES, Result};
use crate::clock;
use crate::protocol::packet::GAME_VERSION;

/// The path of the discovery request after the base URL, up to the game version.
pub(super) const DISCOVERY_PATH_PREFIX: &str = "/api/v1.0/discovery/MinecraftPE/builds/";
/// The path of an issuer's OpenID configuration.
pub(super) const OPENID_PATH: &str = "/.well-known/openid-configuration";

/// An HTTP client for one refresh.
#[derive(Debug)]
pub(super) struct Client {
    agent: ureq::Agent,
    /// The limit on one refresh: the requests it makes share it.
    timeout: Duration,
}

impl Client {
    pub(super) fn new(scheme: Scheme, timeout: Duration) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .build();
        let config = Config::builder()
            .tls_config(tls)
            .https_only(matches!(scheme, Scheme::HttpsOnly))
            .build();
        Self {
            agent: ureq::Agent::with_parts(config, DefaultConnector::default(), InlineResolver),
            timeout,
        }
    }

    /// Fetches discovery, then the OpenID configuration of its issuer, then that JWKS.
    /// The three requests share one timeout.
    pub(super) fn documents(&self, discovery_base: &str) -> Result<(OpenId, Jwks)> {
        let deadline = clock::now() + self.timeout;
        let url = format!(
            "{}{DISCOVERY_PATH_PREFIX}{GAME_VERSION}",
            discovery_base.trim_end_matches('/')
        );
        let issuer = self.get::<Discovery>(&url, deadline)?.into_issuer();
        if issuer.is_empty() {
            return Err(Error::Document {
                url,
                reason: "empty issuer",
            });
        }
        let url = format!("{}{OPENID_PATH}", issuer.trim_end_matches('/'));
        let openid: OpenId = self.get(&url, deadline)?;
        let jwks = self.get(&openid.jwks_uri, deadline)?;
        Ok((openid, jwks))
    }

    /// Fetches the JWKS at `url` within one timeout.
    pub(super) fn jwks(&self, url: &str) -> Result<Jwks> {
        self.get(url, clock::now() + self.timeout)
    }

    /// GETs `url` and parses the body as JSON.
    /// It fails at `deadline` (the DNS lookup excepted, which the OS bounds). The body must be
    /// shorter than [`MAX_DOCUMENT_BYTES`]: ureq refuses one of exactly that size.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: blocking HTTP, called only inside spawn_blocking"
    )]
    fn get<T: DeserializeOwned>(&self, url: &str, deadline: Instant) -> Result<T> {
        let http = |source| Error::Http {
            url: url.to_owned(),
            source,
        };
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(deadline.saturating_duration_since(clock::now())))
            .build()
            .call()
            .map_err(http)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_DOCUMENT_BYTES)
            .read_to_vec()
            .map_err(http)?;
        serde_json::from_slice(&body).map_err(|source| Error::Json {
            url: url.to_owned(),
            source,
        })
    }
}

/// The URL schemes a [`Client`] fetches from.
#[derive(Debug, Clone, Copy)]
pub(super) enum Scheme {
    /// Only `https`, as in production.
    HttpsOnly,
    /// `http` too, so a test can fetch from a local server.
    #[cfg(test)]
    Any,
}

/// ureq's default resolver, always called without a timeout.
/// With one it starts a detached thread per lookup to enforce it. The lookup already runs on the
/// blocking pool, and the OS resolver has its own time limit.
#[derive(Debug)]
struct InlineResolver;

impl Resolver for InlineResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let untimed = NextTimeout {
            after: time::Duration::NotHappening,
            reason: timeout.reason,
        };
        DefaultResolver::default().resolve(uri, config, untimed)
    }
}
