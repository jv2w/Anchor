//! Blocking HTTPS fetches of the authorization documents through ureq (rustls with the aws-lc-rs
//! provider and the webpki roots). Run only on the blocking pool.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout, time};

use super::document::{Discovery, Environment, Jwks, OpenId};
use super::{Error, MAX_DOCUMENT, Result};
use crate::protocol::packet::GAME_VERSION;

/// An HTTP client for one refresh.
#[derive(Debug)]
pub(super) struct Client {
    agent: ureq::Agent,
}

/// ureq's default resolver, always called without a timeout: with one it starts a detached
/// thread per lookup to enforce it. The lookup already runs on the blocking pool, and the OS
/// resolver has its own time limit.
#[derive(Debug)]
struct InlineResolver;

impl Resolver for InlineResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let untimed = NextTimeout {
            after: time::Duration::NotHappening,
            reason: timeout.reason,
        };
        DefaultResolver::default().resolve(uri, config, untimed)
    }
}

impl Client {
    pub(super) fn new(https_only: bool, timeout: Duration) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .build();
        let config = Config::builder()
            .tls_config(tls)
            .https_only(https_only)
            .timeout_global(Some(timeout))
            .build();
        Self {
            agent: ureq::Agent::with_parts(config, DefaultConnector::default(), InlineResolver),
        }
    }

    /// Fetches discovery, then the OpenID configuration of its issuer, then that JWKS.
    pub(super) fn documents(&self, discovery_base: &str) -> Result<(Environment, OpenId, Jwks)> {
        let url = format!(
            "{}/api/v1.0/discovery/MinecraftPE/builds/{GAME_VERSION}",
            discovery_base.trim_end_matches('/')
        );
        let auth = self.get::<Discovery>(&url)?.into_auth();
        if auth.issuer.is_empty() {
            return Err(Error::Document {
                url,
                reason: "empty issuer",
            });
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            auth.issuer.trim_end_matches('/')
        );
        let openid: OpenId = self.get(&url)?;
        let jwks = self.jwks(&openid.jwks_uri)?;
        Ok((auth, openid, jwks))
    }

    pub(super) fn jwks(&self, url: &str) -> Result<Jwks> {
        self.get(url)
    }

    /// GETs `url` and parses the body (at most [`MAX_DOCUMENT`] bytes) as JSON.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: blocking HTTP, called only inside spawn_blocking"
    )]
    fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let http = |source| Error::Http {
            url: url.to_owned(),
            source,
        };
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .call()
            .map_err(http)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_DOCUMENT)
            .read_to_vec()
            .map_err(http)?;
        serde_json::from_slice(&body).map_err(|source| Error::Json {
            url: url.to_owned(),
            source,
        })
    }
}
