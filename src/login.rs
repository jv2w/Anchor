//! Login verification: the multiplayer token, the client data and offline requests.
//!
//! Rules (gophertunnel `minecraft/protocol/login`):
//! - Online: only the `Token` (RS256 by the authorization service, JWKS key by `kid`) proves the
//!   identity; the legacy chain's links are parsed within their limits but never trusted.
//! - Offline: the `Token` claims are read without verification when present, else the legacy
//!   chain (first token self-signed through `x5u`). The XUID is always empty.
//! - Both: the client data JWT is verified (ES384) with the `cpk` key and validated, but its
//!   content stays untrusted.

mod client_data;

pub(crate) use client_data::ClientData;

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

use crate::auth::Keys;
use crate::crypto::{
    self, Algorithm, PublicKey, Verifier, bounded_vec, md5, parse_jws, verify_jws,
};
use crate::protocol::{self, ConnectionRequest, Uuid};

/// The `AuthenticationType` of a guest login.
const GUEST_AUTHENTICATION: u8 = 1;
/// The clock skew allowed on `exp`, `nbf` and `iat`, in seconds (go-jose's default).
const LEEWAY_SECONDS: i64 = 60;
/// The most audience entries read from a token.
const MAX_AUDIENCES: usize = 16;
/// The largest chain JSON (certificate chain and token), in bytes.
/// A real one is under 10 KiB; the cap keeps the parse cheap while the raw client data may use
/// the rest of the batch.
const MAX_CHAIN_JSON_BYTES: usize = 64 * 1024;
/// The most chain links read.
const MAX_CHAIN_LINKS: usize = 8;
/// The largest magnitude of a fractional time claim, in seconds, which a float holds exactly.
const MAX_TIME_SECONDS: f64 = 9.0e15;
/// The largest decoded payload of an identity JWT (the token or a chain link), in bytes.
/// Real ones are about 2 KiB.
pub(crate) const MAX_TOKEN_PAYLOAD_BYTES: usize = 16 * 1024;
/// The longest player name, in bytes.
const MAX_USER_NAME_BYTES: usize = 16;
/// The links of a legacy chain from Mojang.
const MOJANG_CHAIN_LINKS: usize = 3;
/// The audience of multiplayer tokens.
const MULTIPLAYER_AUDIENCE: &str = "api://auth-minecraft-services/multiplayer";
/// The links of a self-signed legacy chain.
const SELF_SIGNED_CHAIN_LINKS: usize = 1;

/// How the identity is proven.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Mode<'a> {
    /// `xbox-auth = true`: the token must verify with these authorization keys.
    Online(&'a Keys),
    /// `xbox-auth = false`: identity claims are taken as sent.
    Offline,
}

/// A verified login.
#[derive(Debug)]
pub(crate) struct Login {
    pub(crate) identity: Identity,
    /// The client's key (`cpk`): it signed the client data and is the encryption peer.
    pub(crate) client_key: PublicKey,
    /// Validated, signed by `client_key`, but written by the client: never trusted.
    pub(crate) client_data: ClientData,
    /// The client data's skin, decoded and checked ([`ClientData::skin`]).
    pub(crate) skin: crate::tick::queue::Skin,
}

/// Who the player is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The Xbox user ID; `None` in offline mode.
    pub(crate) xuid: Option<u64>,
    /// The display name (gamertag online).
    pub(crate) name: String,
    /// The player UUID.
    /// Online it is the token's `leguuid`, else derived from the XUID (gophertunnel
    /// `identityFromXUID`). Offline it is the client's claim: the token's `leguuid`, else derived
    /// from its XUID, else from the name; a legacy chain names it.
    pub(crate) uuid: Uuid,
    /// The PlayFab master player ID (`mid`); empty when not sent.
    pub(crate) playfab_id: String,
}

/// The chain JSON (gophertunnel `parseLoginRequest`).
/// `Certificate` holds the chain as JSON text; older requests put `chain` at the top level
/// instead.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "Certificate", default)]
    certificate: String,
    #[serde(rename = "AuthenticationType", default)]
    authentication_type: u8,
    #[serde(rename = "Token", default)]
    token: String,
    #[serde(default, deserialize_with = "bounded_vec::<_, _, MAX_CHAIN_LINKS>")]
    chain: Vec<String>,
}

impl Envelope {
    fn parse(json: &str) -> Result<Self> {
        if json.len() > MAX_CHAIN_JSON_BYTES {
            return Err(Error::TooLarge("connection request chain"));
        }
        let mut envelope: Self = serde_json::from_str(json).map_err(|source| Error::Json {
            part: "connection request",
            source,
        })?;
        if !envelope.certificate.is_empty() {
            let certificate: Certificate =
                serde_json::from_str(&envelope.certificate).map_err(|source| Error::Json {
                    part: "certificate",
                    source,
                })?;
            envelope.chain = certificate.chain;
        }
        if envelope.authentication_type == GUEST_AUTHENTICATION {
            return Err(Error::Guest);
        }
        Ok(envelope)
    }
}

#[derive(Deserialize)]
struct Certificate {
    #[serde(default, deserialize_with = "bounded_vec::<_, _, MAX_CHAIN_LINKS>")]
    chain: Vec<String>,
}

/// Claims of the multiplayer token (gophertunnel `tokenClaims`).
/// Missing strings are empty, as in Go.
#[derive(Deserialize)]
struct TokenClaims {
    #[serde(default)]
    iss: String,
    #[serde(default)]
    aud: Audience,
    exp: Option<NumericDate>,
    nbf: Option<NumericDate>,
    iat: Option<NumericDate>,
    #[serde(default)]
    cpk: String,
    #[serde(default)]
    xid: String,
    #[serde(default)]
    xname: String,
    #[serde(default)]
    mid: String,
    #[serde(default)]
    leguuid: String,
}

/// Whether `aud` (a string or an array of strings) names the multiplayer audience.
/// Nothing is kept but that answer.
#[derive(Default)]
struct Audience(bool);

impl<'de> Deserialize<'de> for Audience {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct AudienceVisitor;

        impl<'de> Visitor<'de> for AudienceVisitor {
            type Value = Audience;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or an array of strings")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Audience, E> {
                Ok(Audience(v == MULTIPLAYER_AUDIENCE))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Audience, A::Error> {
                let mut found = false;
                let mut count = 0;
                while let Some(entry) = seq.next_element::<String>()? {
                    count += 1;
                    if count > MAX_AUDIENCES {
                        return Err(de::Error::invalid_length(count, &self));
                    }
                    found |= entry == MULTIPLAYER_AUDIENCE;
                }
                Ok(Audience(found))
            }
        }

        d.deserialize_any(AudienceVisitor)
    }
}

/// A JWT time in Unix seconds.
/// Like go-jose, a fractional number is truncated.
#[derive(Clone, Copy)]
pub(crate) struct NumericDate(i64);

impl<'de> Deserialize<'de> for NumericDate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct DateVisitor;

        impl Visitor<'_> for DateVisitor {
            type Value = NumericDate;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of seconds")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<NumericDate, E> {
                Ok(NumericDate(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<NumericDate, E> {
                i64::try_from(v)
                    .map(NumericDate)
                    .map_err(|_| E::custom("time out of range"))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<NumericDate, E> {
                // Truncation toward zero is the intent; the range check rejects NaN and infinity.
                if (-MAX_TIME_SECONDS..=MAX_TIME_SECONDS).contains(&v) {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "consistency: go-jose truncates fractional times the same way"
                    )]
                    Ok(NumericDate(v.trunc() as i64))
                } else {
                    Err(E::custom("time out of range"))
                }
            }
        }

        d.deserialize_any(DateVisitor)
    }
}

/// Claims of a legacy chain link (gophertunnel `identityClaims` and `jwt.Claims`).
#[derive(Deserialize)]
struct ChainClaims {
    exp: Option<NumericDate>,
    nbf: Option<NumericDate>,
    iat: Option<NumericDate>,
    #[serde(rename = "identityPublicKey")]
    identity_public_key: Option<String>,
    #[serde(rename = "extraData")]
    extra_data: Option<ExtraData>,
}

/// gophertunnel `IdentityData` as JSON in the last chain link.
#[derive(Deserialize)]
struct ExtraData {
    #[serde(rename = "XUID", default)]
    xuid: String,
    #[serde(default)]
    identity: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
}

/// Why a login was rejected.
#[derive(Debug)]
pub(crate) enum Error {
    /// The connection request envelope is malformed.
    Envelope(protocol::Error),
    /// A part is larger than its cap.
    TooLarge(&'static str),
    /// JSON of the named part is malformed.
    Json {
        part: &'static str,
        source: serde_json::Error,
    },
    /// Guest authentication (type 1) is not supported (as in gophertunnel).
    Guest,
    /// Online mode needs the multiplayer token.
    MissingToken,
    /// A JWS or key in the named part is malformed.
    Crypto {
        part: &'static str,
        source: crypto::Error,
    },
    /// The named part uses a signature algorithm that is not accepted there.
    Algorithm(&'static str),
    /// The token has no `kid`.
    MissingKeyId,
    /// No authorization key has the token's `kid`.
    /// The session may ask for one JWKS refresh and verify again.
    UnknownKeyId,
    /// The signature of the named part does not verify.
    Signature(&'static str),
    /// `iss` is not the OpenID issuer.
    Issuer,
    /// `aud` lacks the multiplayer audience.
    Audience,
    /// The token has no `exp`.
    MissingExpiry,
    /// `exp` has passed.
    Expired,
    /// `nbf` is in the future.
    NotYetValid,
    /// `iat` is in the future.
    IssuedInFuture,
    /// An identity field breaks a rule.
    Identity(&'static str),
    /// The legacy chain is malformed.
    Chain(&'static str),
    /// A client data field breaks a rule.
    ClientData(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope(_) => f.write_str("malformed connection request"),
            Self::TooLarge(part) => write!(f, "{part} too large"),
            Self::Json { part, .. } => write!(f, "malformed {part} json"),
            Self::Guest => f.write_str("guest logins are refused"),
            Self::MissingToken => f.write_str("missing multiplayer token"),
            Self::Crypto { part, .. } => write!(f, "malformed {part}"),
            Self::Algorithm(part) => write!(f, "{part} signature algorithm not accepted"),
            Self::MissingKeyId => f.write_str("token has no key id"),
            Self::UnknownKeyId => f.write_str("token key id is unknown"),
            Self::Signature(part) => write!(f, "{part} signature does not verify"),
            Self::Issuer => f.write_str("token issuer mismatch"),
            Self::Audience => f.write_str("token audience mismatch"),
            Self::MissingExpiry => f.write_str("token has no expiry"),
            Self::Expired => f.write_str("token expired"),
            Self::NotYetValid => f.write_str("token not valid yet"),
            Self::IssuedInFuture => f.write_str("token issued in the future"),
            Self::Identity(reason) => write!(f, "invalid identity: {reason}"),
            Self::Chain(reason) => write!(f, "invalid login chain: {reason}"),
            Self::ClientData(field) => write!(f, "invalid client data field {field}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Envelope(e) => Some(e),
            Self::Json { source, .. } => Some(source),
            Self::Crypto { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// Verifies a Login packet's connection request.
/// `now` is wall-clock Unix seconds.
pub(crate) fn verify(connection_request: &[u8], mode: Mode<'_>, now: i64) -> Result<Login> {
    let request = ConnectionRequest::parse(connection_request).map_err(Error::Envelope)?;
    let envelope = Envelope::parse(&request.chain)?;
    let (identity, client_key) = match mode {
        Mode::Online(keys) => verify_online_token(&envelope.token, keys, now)?,
        Mode::Offline if !envelope.token.is_empty() => offline_token(&envelope.token)?,
        Mode::Offline => legacy_chain(&envelope.chain, now)?,
    };
    let client_data = ClientData::verify(&request.raw_token, &client_key)?;
    let skin = client_data.skin()?;
    Ok(Login {
        identity,
        client_key,
        client_data,
        skin,
    })
}

/// Verifies a multiplayer token with the online rules.
/// The Login `Token` and the NetherNet offer identity share them. [`Error::UnknownKeyId`] asks
/// for one JWKS refresh.
pub(crate) fn verify_online_token(
    token: &str,
    keys: &Keys,
    now: i64,
) -> Result<(Identity, PublicKey)> {
    if token.is_empty() {
        return Err(Error::MissingToken);
    }
    let jws = parse_jws(token, MAX_TOKEN_PAYLOAD_BYTES).map_err(|source| Error::Crypto {
        part: "token",
        source,
    })?;
    if jws.header.alg != Algorithm::Rs256 {
        return Err(Error::Algorithm("token"));
    }
    let kid = jws.header.kid.as_deref().ok_or(Error::MissingKeyId)?;
    let key = keys.key(kid).ok_or(Error::UnknownKeyId)?;
    verify_jws(&jws, Verifier::Rs256(key)).map_err(|_| Error::Signature("token"))?;
    let claims: TokenClaims =
        serde_json::from_slice(&jws.payload).map_err(|source| Error::Json {
            part: "token",
            source,
        })?;
    if claims.iss != keys.issuer() {
        return Err(Error::Issuer);
    }
    if !claims.aud.0 {
        return Err(Error::Audience);
    }
    let exp = claims.exp.ok_or(Error::MissingExpiry)?;
    check_times(Some(exp), claims.nbf, claims.iat, now)?;
    let xuid = parse_xuid(&claims.xid)?.ok_or(Error::Identity("missing xuid"))?;
    if claims.xname.is_empty() {
        return Err(Error::Identity("empty name"));
    }
    let client_key = client_key(&claims.cpk)?;
    let uuid = match claims.leguuid.as_str() {
        "" => uuid_from_xuid(&claims.xid),
        text => parse_identity_uuid(text)?,
    };
    let identity = Identity {
        xuid: Some(xuid),
        name: claims.xname,
        uuid,
        playfab_id: claims.mid,
    };
    Ok((identity, client_key))
}

fn offline_token(token: &str) -> Result<(Identity, PublicKey)> {
    let jws = parse_jws(token, MAX_TOKEN_PAYLOAD_BYTES).map_err(|source| Error::Crypto {
        part: "token",
        source,
    })?;
    // gophertunnel parses the unverified token with these two algorithms only.
    if !matches!(jws.header.alg, Algorithm::Es384 | Algorithm::Rs256) {
        return Err(Error::Algorithm("token"));
    }
    let claims: TokenClaims =
        serde_json::from_slice(&jws.payload).map_err(|source| Error::Json {
            part: "token",
            source,
        })?;
    let client_key = client_key(&claims.cpk)?;
    parse_xuid(&claims.xid)?;
    let uuid = match (claims.leguuid.as_str(), claims.xid.as_str()) {
        ("", "") => uuid_from_name(&claims.xname),
        ("", xid) => uuid_from_xuid(xid),
        (text, _) => parse_identity_uuid(text)?,
    };
    check_user_name(&claims.xname)?;
    let identity = Identity {
        xuid: None,
        name: claims.xname,
        uuid,
        playfab_id: claims.mid,
    };
    Ok((identity, client_key))
}

/// Reads the legacy chain (gophertunnel `parseLegacyChain`) for offline mode.
/// Each link verifies with the key the previous one named, starting from the first link's `x5u`.
fn legacy_chain(chain: &[String], now: i64) -> Result<(Identity, PublicKey)> {
    if !matches!(chain.len(), SELF_SIGNED_CHAIN_LINKS | MOJANG_CHAIN_LINKS) {
        return Err(Error::Chain("not 1 or 3 links"));
    }
    let last = chain.len() - 1;
    let mut key = None;
    let mut extra = None;
    for (i, link) in chain.iter().enumerate() {
        let jws = parse_jws(link, MAX_TOKEN_PAYLOAD_BYTES).map_err(|source| Error::Crypto {
            part: "chain",
            source,
        })?;
        if jws.header.alg != Algorithm::Es384 {
            return Err(Error::Algorithm("chain"));
        }
        let current = match key.take() {
            Some(key) => key,
            None => client_key(jws.header.x5u.as_deref().ok_or(Error::Chain("no x5u"))?)?,
        };
        verify_jws(&jws, Verifier::Es384(&current)).map_err(|_| Error::Signature("chain"))?;
        let claims: ChainClaims =
            serde_json::from_slice(&jws.payload).map_err(|source| Error::Json {
                part: "chain",
                source,
            })?;
        check_times(claims.exp, claims.nbf, claims.iat, now)?;
        key = Some(match claims.identity_public_key {
            Some(next) => client_key(&next)?,
            None => current,
        });
        if i == last {
            extra = claims.extra_data;
        }
    }
    #[expect(
        clippy::expect_used,
        reason = "invariant: a chain has a link at least, and every link leaves a key"
    )]
    let key = key.expect("a verified chain leaves a key");
    let extra = extra.ok_or(Error::Chain("no identity data"))?;
    parse_xuid(&extra.xuid)?;
    let uuid = parse_identity_uuid(&extra.identity)?;
    check_user_name(&extra.display_name)?;
    let identity = Identity {
        xuid: None,
        name: extra.display_name,
        uuid,
        playfab_id: String::new(),
    };
    Ok((identity, key))
}

/// Checks `exp`, `nbf` and `iat` against `now`.
/// It allows [`LEEWAY_SECONDS`] of skew (go-jose `ValidateWithLeeway`).
pub(crate) fn check_times(
    exp: Option<NumericDate>,
    nbf: Option<NumericDate>,
    iat: Option<NumericDate>,
    now: i64,
) -> Result<()> {
    if let Some(NumericDate(nbf)) = nbf
        && now.saturating_add(LEEWAY_SECONDS) < nbf
    {
        return Err(Error::NotYetValid);
    }
    if let Some(NumericDate(exp)) = exp
        && now.saturating_sub(LEEWAY_SECONDS) > exp
    {
        return Err(Error::Expired);
    }
    if let Some(NumericDate(iat)) = iat
        && now.saturating_add(LEEWAY_SECONDS) < iat
    {
        return Err(Error::IssuedInFuture);
    }
    Ok(())
}

fn client_key(cpk: &str) -> Result<PublicKey> {
    PublicKey::from_base64_spki(cpk).map_err(|source| Error::Crypto {
        part: "client key",
        source,
    })
}

/// An XUID is empty or decimal digits that fit an `i64` (gophertunnel `strconv.ParseInt`).
fn parse_xuid(xid: &str) -> Result<Option<u64>> {
    if xid.is_empty() {
        return Ok(None);
    }
    if !xid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Identity("xuid not numeric"));
    }
    xid.parse::<i64>()
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .map(Some)
        .ok_or(Error::Identity("xuid out of range"))
}

/// Checks that `name` can be a player's name.
/// It is 1 to 16 bytes of `A-Z a-z 0-9 _` and space only, and not `rcon` or `console` in any
/// case. Spaces may lead, trail or repeat. The error says which rule `name` breaks.
/// Logins without Xbox sign-in and the commands that take a player's name (`/op`, `/whitelist`)
/// check it.
pub(crate) fn check_user_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_USER_NAME_BYTES {
        return Err(Error::Identity("name must be 1 to 16 bytes"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b' ')
    {
        return Err(Error::Identity(
            "name has characters other than a-z, A-Z, 0-9, _ and space",
        ));
    }
    if name.eq_ignore_ascii_case("rcon") || name.eq_ignore_ascii_case("console") {
        return Err(Error::Identity("name is reserved"));
    }
    Ok(())
}

/// The identity UUID (gophertunnel requires a parseable, non-nil UUID).
fn parse_identity_uuid(text: &str) -> Result<Uuid> {
    text.parse::<Uuid>()
        .ok()
        .filter(|u| *u != Uuid::default())
        .ok_or(Error::Identity("uuid"))
}

/// gophertunnel `identityFromXUID`.
fn uuid_from_xuid(xuid: &str) -> Uuid {
    uuid_v3("pocket-auth-1-xuid:", xuid)
}

/// The UUID of an offline token that has neither `leguuid` nor `xid`.
/// gophertunnel draws a random one; Anchor derives it from the name so the same player keeps the
/// same UUID (the Java Edition offline-player rule).
fn uuid_from_name(name: &str) -> Uuid {
    uuid_v3("OfflinePlayer:", name)
}

/// A name-based version 3 UUID: MD5 of `prefix ‖ name` with the version and variant bits set.
/// Those bits are in bytes 6 and 8 (RFC 4122).
fn uuid_v3(prefix: &str, name: &str) -> Uuid {
    let mut id = md5(&[prefix.as_bytes(), name.as_bytes()]);
    id[6] = (id[6] & 0x0f) | 0x30;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::OnceLock;

    use serde_json::{Value, json};

    use crate::auth::testing::{ISSUER, Issuer, KID};
    use crate::crypto::{RAW_URL, STD, ServerKey, sign_jwt};
    use crate::protocol::fuzz;

    const NOW: i64 = 1_790_000_000;
    const XID: &str = "2535412345678901";

    static PLAYER: OnceLock<ServerKey> = OnceLock::new();

    fn player() -> &'static ServerKey {
        PLAYER.get_or_init(|| ServerKey::generate().unwrap())
    }

    fn claims() -> Value {
        json!({
            "iss": ISSUER,
            "aud": [MULTIPLAYER_AUDIENCE],
            "exp": NOW + 3600,
            "nbf": NOW - 60,
            "iat": NOW - 60,
            "cpk": player().public_key_base64(),
            "xid": XID,
            "xname": "Steve",
            "mid": "ABCDEF",
            "tid": "20CA2",
            "ipt": "PlayFab",
        })
    }

    fn envelope(chain: &str, raw_token: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for part in [chain, raw_token] {
            out.extend_from_slice(&i32::try_from(part.len()).unwrap().to_le_bytes());
            out.extend_from_slice(part.as_bytes());
        }
        out
    }

    fn client_data_json() -> Value {
        json!({
            "DeviceOS": 7,
            "DeviceId": "a4f365bb1e04459bbe4cb3cbf9d546e0",
            "GameVersion": "1.26.50",
            "LanguageCode": "en_US",
            "ServerAddress": "127.0.0.1:19132",
            "SelfSignedId": "05601fd2-9c71-30b1-b174-5fa11b9de09f",
            "SkinData": STD.encode(&[1, 2, 3, 4, 5, 6, 7, 8]),
            "SkinImageHeight": 1,
            "SkinImageWidth": 2,
            "SkinId": "skin",
            "SkinResourcePatch": STD.encode(br#"{"geometry":{"default":"geometry.humanoid"}}"#),
            "SkinGeometryData": "",
            "PlayFabId": "0123456789abcdef",
            "PersonaPieces": [{"IsDefault": true, "PackId": "p", "PieceId": "q", "PieceType": "persona_body", "ProductId": ""}],
            "PieceTintColors": [{"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_eyes"}],
            "AnimatedImageData": [{"Frames": 2.0, "Image": STD.encode(&[0; 8]), "ImageHeight": 2, "ImageWidth": 1, "Type": 1, "AnimationExpression": 0}],
            "UIProfile": 0,
            "IsPartyLeader": false,
        })
    }

    fn online_request(token: &str, data: &Value, signer: &ServerKey) -> Vec<u8> {
        let chain = json!({
            "AuthenticationType": 0,
            "Certificate": json!({"chain": []}).to_string(),
            "Token": token,
        });
        envelope(&chain.to_string(), &sign_jwt(signer, data).unwrap())
    }

    fn online(token: &str) -> Result<Login> {
        let request = online_request(token, &client_data_json(), player());
        verify(&request, Mode::Online(&Issuer::get().keys()), NOW)
    }

    fn with(mut claims: Value, key: &str, value: Value) -> Value {
        claims[key] = value;
        claims
    }

    #[test]
    fn a_test_issuer_token_passes() {
        let login = online(&Issuer::get().token(&claims())).unwrap();
        assert_eq!(login.identity.xuid, Some(2_535_412_345_678_901));
        assert_eq!(login.identity.name, "Steve");
        assert_eq!(login.identity.uuid, uuid_from_xuid(XID));
        assert_eq!(login.identity.playfab_id, "ABCDEF");
        assert_eq!(
            STD.encode(login.client_key.spki()),
            player().public_key_base64()
        );
        assert_eq!(login.client_data.skin_id, "skin");
        assert_eq!(login.client_data.device_os, 7);
        // `aud` may be a single string; times at the edge of the leeway pass.
        let edge = with(
            with(claims(), "exp", json!(NOW - 60)),
            "nbf",
            json!(NOW + 60),
        );
        let edge = with(
            with(edge, "iat", json!(NOW + 60)),
            "aud",
            json!(MULTIPLAYER_AUDIENCE),
        );
        online(&Issuer::get().token(&edge)).unwrap();
        let float_times = with(claims(), "exp", json!(1.0e10));
        online(&Issuer::get().token(&float_times)).unwrap();
        let leguuid = with(
            claims(),
            "leguuid",
            json!("6f0a7c29-2c7f-4bc5-8b8e-3d0ad1f1e5a1"),
        );
        let login = online(&Issuer::get().token(&leguuid)).unwrap();
        assert_eq!(
            login.identity.uuid,
            "6f0a7c29-2c7f-4bc5-8b8e-3d0ad1f1e5a1".parse().unwrap()
        );
    }

    type Check = fn(&Error) -> bool;

    fn without(mut claims: Value, key: &str) -> Value {
        claims.as_object_mut().unwrap().remove(key);
        claims
    }

    #[test]
    fn each_broken_token_is_rejected() {
        let issuer = Issuer::get();
        let other = Issuer::generate();
        let token = issuer.token(&claims());
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let mut sig = RAW_URL.decode(signature, 1024).unwrap();
        sig[10] ^= 1;
        let bad_signature = format!("{signed}.{}", RAW_URL.encode(&sig));
        let unsigned = |header: Value| {
            format!(
                "{}.{}.",
                RAW_URL.encode(header.to_string().as_bytes()),
                RAW_URL.encode(claims().to_string().as_bytes())
            )
        };
        let es384 = {
            let header = json!({"alg": "ES384", "kid": KID});
            let mut token = unsigned(header);
            let signature = player()
                .sign_es384(token.trim_end_matches('.').as_bytes())
                .unwrap();
            token.push_str(&RAW_URL.encode(&signature));
            token
        };
        let cases: Vec<(&str, String, Check)> = vec![
            ("bad signature", bad_signature, |e| matches!(e, Error::Signature("token"))),
            ("other issuer key", other.token(&claims()), |e| matches!(e, Error::Signature("token"))),
            ("unknown kid", issuer.sign(&json!({"alg": "RS256", "kid": "nope"}), &claims()), |e| matches!(e, Error::UnknownKeyId)),
            ("no kid", issuer.sign(&json!({"alg": "RS256"}), &claims()), |e| matches!(e, Error::MissingKeyId)),
            ("alg none", unsigned(json!({"alg": "none", "kid": KID})), |e| matches!(e, Error::Algorithm("token"))),
            ("alg HS256", issuer.sign(&json!({"alg": "HS256", "kid": KID}), &claims()), |e| matches!(e, Error::Algorithm("token"))),
            ("alg ES384", es384, |e| matches!(e, Error::Algorithm("token"))),
            ("wrong iss", issuer.token(&with(claims(), "iss", json!("https://issuer.test"))), |e| matches!(e, Error::Issuer)),
            ("wrong aud", issuer.token(&with(claims(), "aud", json!(["api://other"]))), |e| matches!(e, Error::Audience)),
            ("no aud", issuer.token(&without(claims(), "aud")), |e| matches!(e, Error::Audience)),
            ("expired", issuer.token(&with(claims(), "exp", json!(NOW - 61))), |e| matches!(e, Error::Expired)),
            ("no exp", issuer.token(&without(claims(), "exp")), |e| matches!(e, Error::MissingExpiry)),
            ("future nbf", issuer.token(&with(claims(), "nbf", json!(NOW + 61))), |e| matches!(e, Error::NotYetValid)),
            ("future iat", issuer.token(&with(claims(), "iat", json!(NOW + 61))), |e| matches!(e, Error::IssuedInFuture)),
            ("malformed cpk", issuer.token(&with(claims(), "cpk", json!("AAAA"))), |e| matches!(e, Error::Crypto { part: "client key", .. })),
            ("p-256 cpk", issuer.token(&with(claims(), "cpk", json!("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEQiBIUwHqzdOfK1C8U+8lHzCC5rbVp+rjwK3dP3nOxMjDA1Q5c3hu2KcsPFm9CnCUZuSyhDKH7nrSvx3V4l4hRQ=="))), |e| matches!(e, Error::Crypto { part: "client key", .. })),
            ("letters in xid", issuer.token(&with(claims(), "xid", json!("12a"))), |e| matches!(e, Error::Identity(_))),
            ("no xid", issuer.token(&without(claims(), "xid")), |e| matches!(e, Error::Identity(_))),
            ("empty xname", issuer.token(&with(claims(), "xname", json!(""))), |e| matches!(e, Error::Identity(_))),
            ("nil leguuid", issuer.token(&with(claims(), "leguuid", json!("00000000-0000-0000-0000-000000000000"))), |e| matches!(e, Error::Identity(_))),
            ("no token", String::new(), |e| matches!(e, Error::MissingToken)),
            ("not a jws", "a.b".to_owned(), |e| matches!(e, Error::Crypto { part: "token", .. })),
        ];
        for (name, token, expected) in cases {
            let err = online(&token).unwrap_err();
            assert!(expected(&err), "{name}: {err:?}");
        }
    }

    #[test]
    fn client_data_signed_by_another_key_is_rejected() {
        let token = Issuer::get().token(&claims());
        let other = ServerKey::generate().unwrap();
        let request = online_request(&token, &client_data_json(), &other);
        let err = verify(&request, Mode::Online(&Issuer::get().keys()), NOW).unwrap_err();
        assert!(matches!(err, Error::Signature("client data")), "{err:?}");
    }

    #[test]
    fn invalid_client_data_is_rejected() {
        let cases = [
            ("DeviceOS", json!(0)),
            ("DeviceOS", json!(16)),
            ("GameVersion", json!("1.26.50-beta")),
            ("GameVersion", json!("")),
            ("LanguageCode", json!("e")),
            ("LanguageCode", json!("en__US")),
            ("PlatformOfflineId", json!("not-a-uuid")),
            ("PlatformOnlineId", json!("-1")),
            ("SelfSignedId", json!("x")),
            ("ServerAddress", json!("127.0.0.1")),
            ("ServerAddress", json!("host name:19132")),
            ("ServerAddress", json!("127.0.0.1:70000")),
            ("ServerAddress", json!("https://example.com:443:444")),
            ("ServerAddress", json!("https://example.com")),
            ("ServerAddress", json!("https://:7551")),
            ("ServerAddress", json!("https://[]:7551")),
            ("ServerAddress", json!("ftp://example.com:21:21")),
            ("ServerAddress", json!("ftp://example.com:21")),
            ("ServerAddress", json!("[::1:19132")),
            ("SkinData", json!(STD.encode(&[0; 7]))),
            ("SkinData", json!("AAAA!")),
            ("CapeData", json!("AAAA")),
            ("PlayFabId", json!("ABCDEF")),
            ("SkinGeometryData", json!(STD.encode(b"[1]"))),
            ("SkinGeometryData", json!("%%%")),
            ("SkinResourcePatch", json!("")),
            ("SkinResourcePatch", json!(STD.encode(b"{"))),
            ("SkinId", json!("")),
            ("UIProfile", json!(3)),
            ("DeviceOS", json!("7")),
        ];
        let token = Issuer::get().token(&claims());
        let keys = Issuer::get().keys();
        for (field, value) in cases {
            let data = with(client_data_json(), field, value.clone());
            let request = online_request(&token, &data, player());
            let err = verify(&request, Mode::Online(&keys), NOW).unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::ClientData(_)
                        | Error::Json {
                            part: "client data",
                            ..
                        }
                ),
                "{field} = {value}: {err:?}"
            );
        }
        let mut animation = client_data_json();
        animation["AnimatedImageData"][0]["Type"] = json!(4);
        let request = online_request(&token, &animation, player());
        assert!(verify(&request, Mode::Online(&keys), NOW).is_err());
        let mut pieces = client_data_json();
        pieces["PersonaPieces"] = json!(vec![json!({}); client_data::MAX_PIECES + 1]);
        let request = online_request(&token, &pieces, player());
        assert!(verify(&request, Mode::Online(&keys), NOW).is_err());
    }

    /// Client data texts the player keeps for its whole stay are bounded.
    #[test]
    fn client_data_texts_past_their_caps_are_rejected() {
        let long = "1".repeat(4096);
        let cases = [
            ("DeviceId", json!(long)),
            ("DeviceModel", json!(long)),
            ("GameVersion", json!(long)),
            ("ServerAddress", json!(format!("http://{long}:7551"))),
        ];
        let token = Issuer::get().token(&claims());
        let keys = Issuer::get().keys();
        for (field, value) in cases {
            let data = with(client_data_json(), field, value);
            let request = online_request(&token, &data, player());
            let err = verify(&request, Mode::Online(&keys), NOW).unwrap_err();
            assert!(
                matches!(err, Error::ClientData(name) if name == field),
                "{field}: {err:?}"
            );
        }
    }

    #[test]
    fn the_client_skin_is_decoded_as_sent_and_bounded() {
        let token = Issuer::get().token(&claims());
        let keys = Issuer::get().keys();
        let login = |data: &Value| {
            let request = online_request(&token, data, player());
            verify(&request, Mode::Online(&keys), NOW)
        };
        let geometry = br#"{"format_version":"1.12.0","minecraft:geometry":[]}"#;
        let mut data = client_data_json();
        for (field, value) in [
            ("SkinGeometryData", json!(STD.encode(geometry))),
            (
                "SkinGeometryDataEngineVersion",
                json!(STD.encode(b"1.14.0")),
            ),
            ("SkinAnimationData", json!(STD.encode(b"animation"))),
            ("CapeData", json!(STD.encode(&[9; 8]))),
            ("CapeImageWidth", json!(2)),
            ("CapeImageHeight", json!(1)),
            ("CapeId", json!("cape")),
            ("ArmSize", json!("slim")),
            ("SkinColor", json!("#b37b62")),
            ("PersonaSkin", json!(true)),
            ("PremiumSkin", json!(true)),
            ("TrustedSkin", json!(true)),
            ("OverrideSkin", json!(true)),
            ("CapeOnClassicSkin", json!(true)),
            ("ProfileHash", json!("hash")),
        ] {
            data[field] = value;
        }
        data["PersonaPieces"][0]["PackId"] = json!("05601fd2-9c71-30b1-b174-5fa11b9de09f");
        let login_skin = login(&data).unwrap().skin;
        let skin = login_skin.get();
        assert_eq!(skin.skin_id, "skin");
        assert_eq!(
            skin.skin_resource_patch,
            br#"{"geometry":{"default":"geometry.humanoid"}}"#
        );
        assert_eq!((skin.skin_image_width, skin.skin_image_height), (2, 1));
        assert_eq!(skin.skin_data, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(skin.skin_geometry, geometry);
        assert_eq!(skin.geometry_data_engine_version, b"1.14.0");
        assert_eq!(skin.animation_data, b"animation");
        assert_eq!((skin.cape_image_width, skin.cape_image_height), (2, 1));
        assert_eq!(skin.cape_data, [9; 8]);
        assert_eq!(
            (skin.cape_id.as_str(), skin.full_id.as_str()),
            ("cape", "skincape")
        );
        assert_eq!(skin.animations.len(), 1);
        let animation = &skin.animations[0];
        assert_eq!((animation.image_width, animation.image_height), (1, 2));
        assert_eq!(animation.image_data, [0; 8]);
        assert_eq!((animation.animation_type, animation.frame_count), (1, 2.0));
        assert_eq!(skin.arm_size, 0, "slim");
        let color = skin.skin_colour;
        assert_eq!(
            (color.r, color.g, color.b, color.a),
            (0xb3, 0x7b, 0x62, 0xff)
        );
        assert_eq!(skin.persona_pieces[0].piece_type, 2, "persona_body");
        assert_eq!(
            skin.persona_pieces[0].pack_id,
            "05601fd2-9c71-30b1-b174-5fa11b9de09f".parse().unwrap()
        );
        assert_eq!(skin.piece_tint_colours[0].piece_type, "persona_eyes");
        assert_eq!(skin.piece_tint_colours[0].colours[1].b, 2);
        assert!(skin.premium_skin && skin.persona_skin && skin.persona_cape_on_classic_skin);
        assert!(skin.trusted && skin.override_appearance && skin.primary_user);
        assert_eq!(skin.profile_hash, "hash");
        assert_eq!(protocol::skin_validate(skin), None);
        // A 1024 × 1024 image fills the 4 MiB bound; one byte more in any field is refused.
        let mut big = data.clone();
        big["SkinData"] = json!(STD.encode(&vec![0; 1024 * 1024 * 4]));
        big["SkinImageWidth"] = json!(1024);
        big["SkinImageHeight"] = json!(1024);
        big["SkinResourcePatch"] = json!(STD.encode(b"{}"));
        big["SkinGeometryData"] = json!("");
        big["SkinGeometryDataEngineVersion"] = json!("");
        big["SkinAnimationData"] = json!("");
        big["CapeData"] = json!("");
        big["CapeImageWidth"] = json!(0);
        big["CapeImageHeight"] = json!(0);
        big["AnimatedImageData"] = json!([]);
        let err = login(&big).unwrap_err();
        assert!(matches!(err, Error::ClientData("Skin")), "{err:?}");
        // Dimensions that multiply right but do not fit, and frames that are not finite.
        let mut negative = data.clone();
        negative["SkinImageWidth"] = json!(-2);
        negative["SkinImageHeight"] = json!(-1);
        assert!(matches!(
            login(&negative).unwrap_err(),
            Error::ClientData("Skin")
        ));
        let mut frames = data;
        frames["AnimatedImageData"][0]["Frames"] = json!(1.0e300);
        assert!(matches!(
            login(&frames).unwrap_err(),
            Error::ClientData("Skin")
        ));
    }

    #[test]
    fn coco_persona_pieces_go_out_as_piece_type_28() {
        let mut data = client_data_json();
        data["PersonaPieces"] = json!([
            {"IsDefault": true, "PackId": "p", "PieceId": "q", "PieceType": "persona_coco", "ProductId": ""},
        ]);
        let request = online_request(&Issuer::get().token(&claims()), &data, player());
        let login = verify(&request, Mode::Online(&Issuer::get().keys()), NOW).unwrap();
        let mut w = protocol::Writer::new(Vec::new(), 0);
        protocol::Codec::encode(&login.skin.get().persona_pieces[0], &mut w).unwrap();
        let bytes = w.as_bytes();
        // The piece ID string "q", then the piece type as a little-endian u32.
        assert_eq!(bytes[..6], [1, b'q', 28, 0, 0, 0]);
    }

    #[test]
    fn login_skins_go_out_in_the_form_clients_read() {
        let mut data = client_data_json();
        data["PieceTintColors"] = json!([
            {"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_facial_hair"},
            {"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_right_arm"},
            {"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_high_pants"},
            {"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_hand"},
            {"Colors": ["#1", "#2", "#3", "#4"], "PieceType": "persona_eyes"},
        ]);
        let request = online_request(&Issuer::get().token(&claims()), &data, player());
        let login = verify(&request, Mode::Online(&Issuer::get().keys()), NOW).unwrap();
        let skin = login.skin.get();
        assert_eq!(skin.skin_geometry, b"{}", "an empty login geometry is none");
        let names: Vec<Vec<u8>> = skin
            .piece_tint_colours
            .iter()
            .map(|t| {
                let mut w = protocol::Writer::new(Vec::new(), 0);
                protocol::Codec::encode(t, &mut w).unwrap();
                let bytes = w.as_bytes();
                bytes[1..=usize::from(bytes[0])].to_vec()
            })
            .collect();
        assert_eq!(
            names,
            [
                &b"facialhair"[..],
                b"rightarm",
                b"high_pants",
                b"hands",
                b"eyes"
            ]
        );
    }

    #[test]
    fn skin_texts_count_toward_the_skin_bound() {
        // The skin ID and cape ID are held twice (alone and in the full ID): the texts alone
        // take twice the bound while every image is empty.
        let half = crate::entity::MAX_SKIN_BYTES / 2;
        let mut data = client_data_json();
        data["SkinId"] = json!("a".repeat(half));
        data["CapeId"] = json!("b".repeat(half));
        data["SkinData"] = json!("");
        data["SkinImageWidth"] = json!(0);
        data["SkinImageHeight"] = json!(0);
        data["SkinResourcePatch"] = json!(STD.encode(b"null"));
        data["AnimatedImageData"] = json!([]);
        let request = online_request(&Issuer::get().token(&claims()), &data, player());
        let err = verify(&request, Mode::Online(&Issuer::get().keys()), NOW).err();
        assert!(matches!(err, Some(Error::ClientData("Skin"))), "{err:?}");
    }

    #[test]
    fn valid_client_data_variants_pass() {
        let cases = [
            ("ServerAddress", json!("[::1]:19132")),
            ("ServerAddress", json!("::1:19132")),
            ("ServerAddress", json!("[fe80::1%eth0]:19132")),
            ("ServerAddress", json!("play.example.com:19132")),
            ("ServerAddress", json!("https://example.com:7551:7551")),
            ("ServerAddress", json!("http://[::1]:7551/path:7551")),
            ("ServerAddress", json!("https://example.com:7551")),
            ("ServerAddress", json!("http://[::1]:7551/path")),
            ("LanguageCode", json!("zh-Hans_CN")),
            ("LanguageCode", json!("ko_KR")),
            (
                "PlatformOfflineId",
                json!("{05601fd2-9c71-30b1-b174-5fa11b9de09f}"),
            ),
            ("PlatformOnlineId", json!("18446744073709551615")),
            (
                "SkinGeometryData",
                json!(STD.encode(b"{\"format_version\":\"1.12.0\"}")),
            ),
            ("SkinResourcePatch", json!(STD.encode(b"null"))),
            ("CapeData", json!("")),
        ];
        let token = Issuer::get().token(&claims());
        for (field, value) in cases {
            let data = with(client_data_json(), field, value.clone());
            let request = online_request(&token, &data, player());
            let login = verify(&request, Mode::Online(&Issuer::get().keys()), NOW);
            assert!(login.is_ok(), "{field} = {value}: {login:?}");
        }
        let data = with(client_data_json(), "ServerAddress", json!("::1:19132"));
        let request = online_request(&token, &data, player());
        let login = verify(&request, Mode::Online(&Issuer::get().keys()), NOW).unwrap();
        assert_eq!(login.client_data.server_address, "[::1]:19132");
    }

    #[test]
    fn envelopes_are_checked() {
        let token = Issuer::get().token(&claims());
        let raw = sign_jwt(player(), &client_data_json()).unwrap();
        let keys = Issuer::get().keys();
        let guest = json!({"AuthenticationType": 1, "Token": token}).to_string();
        let err = verify(&envelope(&guest, &raw), Mode::Online(&keys), NOW).unwrap_err();
        assert!(matches!(err, Error::Guest));
        // The legacy layout: `chain` at the top level, no `Certificate`.
        let top_level = json!({"chain": [], "Token": token}).to_string();
        verify(&envelope(&top_level, &raw), Mode::Online(&keys), NOW).unwrap();
        let huge = format!("{{\"Token\":\"{}\"}}", "a".repeat(MAX_CHAIN_JSON_BYTES));
        let err = verify(&envelope(&huge, &raw), Mode::Online(&keys), NOW).unwrap_err();
        assert!(matches!(err, Error::TooLarge(_)));
        let long_chain =
            json!({"Certificate": json!({"chain": vec![""; 9]}).to_string(), "Token": token});
        let err = verify(
            &envelope(&long_chain.to_string(), &raw),
            Mode::Online(&keys),
            NOW,
        );
        assert!(matches!(
            err,
            Err(Error::Json {
                part: "certificate",
                ..
            })
        ));
        let err = verify(b"\x01\x00", Mode::Online(&keys), NOW).unwrap_err();
        assert!(matches!(err, Error::Envelope(_)));
    }

    #[test]
    fn offline_names_follow_the_name_rules() {
        for name in [
            "a",
            "_",
            "A_1",
            "Sixteen_Letters1",
            " Steve",
            "Steve ",
            "Ste  ve",
            "1Steve",
            "                ",
            "rcon1",
            "consoles",
        ] {
            assert!(check_user_name(name).is_ok(), "{name:?}");
        }
        for name in [
            "",
            "SeventeenLetters1",
            "Stéve",
            "스티브",
            "§cAdmin",
            "Ste\u{0}ve",
            "Ste\tve",
            "Ste-ve",
            "Ste.ve",
            "rcon",
            "RCON",
            "console",
            "CONSOLE",
            "Console",
        ] {
            assert!(
                matches!(check_user_name(name), Err(Error::Identity(_))),
                "{name:?}"
            );
        }
        // Bytes, not characters: 8 two-byte letters are 16 bytes, and still not ASCII.
        assert!(check_user_name(&"é".repeat(8)).is_err());
    }

    /// A self-signed one-link legacy chain, as gophertunnel `EncodeOffline(.., legacy = true)`.
    fn legacy_chain_request(name: &str, identity: &str, exp: i64) -> Vec<u8> {
        let key = player();
        let link = sign_jwt(
            key,
            &json!({
                "exp": exp,
                "nbf": NOW - 3600,
                "identityPublicKey": key.public_key_base64(),
                "extraData": {"XUID": "", "identity": identity, "displayName": name},
            }),
        )
        .unwrap();
        let chain = json!({"chain": [link]}).to_string();
        envelope(&chain, &sign_jwt(key, &client_data_json()).unwrap())
    }

    #[test]
    fn offline_requests_follow_gophertunnel_rules() {
        let uuid = "6f0a7c29-2c7f-4bc5-8b8e-3d0ad1f1e5a1";
        let login = verify(
            &legacy_chain_request("Steve 2", uuid, NOW + 60),
            Mode::Offline,
            NOW,
        )
        .unwrap();
        assert_eq!(login.identity.xuid, None);
        assert_eq!(login.identity.name, "Steve 2");
        assert_eq!(login.identity.uuid, uuid.parse().unwrap());
        let expired = legacy_chain_request("Steve", uuid, NOW - 61);
        assert!(matches!(
            verify(&expired, Mode::Offline, NOW),
            Err(Error::Expired)
        ));
        for name in [
            "",
            "Stéve",
            "Ste\u{0}ve",
            "SeventeenLetters1",
            "§cAdmin",
            "CONSOLE",
        ] {
            let request = legacy_chain_request(name, uuid, NOW + 60);
            let err = verify(&request, Mode::Offline, NOW).unwrap_err();
            assert!(matches!(err, Error::Identity(_)), "{name:?}: {err:?}");
        }
        for name in [" Steve_", "1Steve", "A1  b2"] {
            verify(
                &legacy_chain_request(name, uuid, NOW + 60),
                Mode::Offline,
                NOW,
            )
            .unwrap();
        }
        let nil = legacy_chain_request("Steve", "00000000-0000-0000-0000-000000000000", NOW + 60);
        assert!(matches!(
            verify(&nil, Mode::Offline, NOW),
            Err(Error::Identity(_))
        ));

        // An unverified token: the XUID is dropped, the UUID comes from leguuid, xid or the name.
        let raw = sign_jwt(player(), &client_data_json()).unwrap();
        let offline = |claims: &Value| {
            let chain = json!({"Certificate": json!({"chain": [""]}).to_string(), "Token": Issuer::generate().token(claims)});
            verify(&envelope(&chain.to_string(), &raw), Mode::Offline, NOW)
        };
        let login = offline(&claims()).unwrap();
        assert_eq!(login.identity.xuid, None);
        assert_eq!(login.identity.uuid, uuid_from_xuid(XID));
        let login = offline(&without(claims(), "xid")).unwrap();
        assert_eq!(login.identity.uuid, uuid_from_name("Steve"));
        assert!(matches!(
            offline(&with(claims(), "xname", json!("§"))),
            Err(Error::Identity(_))
        ));
        let expired = with(claims(), "exp", json!(0));
        offline(&expired).unwrap();
    }

    #[test]
    fn name_uuids_match_known_values() {
        // Java Edition's offline UUID for "Notch" uses the same rule.
        let notch = uuid_from_name("Notch");
        assert_eq!(
            notch,
            "b50ad385-829d-3141-a216-7e7d7539ba7f".parse().unwrap()
        );
    }

    #[derive(Deserialize)]
    struct Golden {
        request: String,
        name: String,
        uuid: String,
        xuid: String,
        skin_id: String,
        client_spki: String,
        #[serde(default)]
        kid: String,
        #[serde(default)]
        issuer: String,
        #[serde(default)]
        n: String,
        #[serde(default)]
        e: String,
        #[serde(default)]
        now: i64,
    }

    /// The goldens in `testdata/requests.json`.
    /// They come from a Go module that requires the pinned gophertunnel:
    /// `login.EncodeOffline` (token and legacy forms) and `login.Encode` with a multiplayer token
    /// that a Go RSA test issuer signed (`kid`, `n`, `e`, `issuer` recorded), each parsed back
    /// with gophertunnel `login.Parse` for the expected identity. ECDSA signatures are randomized,
    /// so the goldens are verify-only; `generated_at` and `now` fix the clock for their time
    /// claims.
    #[derive(Deserialize)]
    struct Goldens {
        generated_at: i64,
        offline: Golden,
        offline_legacy: Golden,
        online: Golden,
    }

    fn goldens() -> Goldens {
        serde_json::from_str(include_str!("login/testdata/requests.json")).unwrap()
    }

    fn check(golden: &Golden, login: &Login) {
        assert_eq!(login.identity.name, golden.name);
        assert_eq!(login.identity.uuid, golden.uuid.parse().unwrap());
        assert_eq!(
            login
                .identity
                .xuid
                .map(|x| x.to_string())
                .unwrap_or_default(),
            golden.xuid
        );
        assert_eq!(login.client_data.skin_id, golden.skin_id);
        assert_eq!(STD.encode(login.client_key.spki()), golden.client_spki);
    }

    #[test]
    fn gophertunnel_offline_requests_pass() {
        let g = goldens();
        for golden in [&g.offline, &g.offline_legacy] {
            let request = STD.decode(&golden.request, 1 << 20).unwrap();
            let login = verify(&request, Mode::Offline, g.generated_at).unwrap();
            check(golden, &login);
        }
        // Online mode never accepts a self-signed token.
        let request = STD.decode(&g.offline.request, 1 << 20).unwrap();
        let err = verify(
            &request,
            Mode::Online(&Issuer::get().keys()),
            g.generated_at,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Algorithm("token")), "{err:?}");
    }

    #[test]
    fn a_gophertunnel_online_request_passes_with_its_issuer_keys() {
        let g = goldens().online;
        let keys = Issuer::keys_from(&g.issuer, &g.kid, &g.n, &g.e);
        let request = STD.decode(&g.request, 1 << 20).unwrap();
        let login = verify(&request, Mode::Online(&keys), g.now).unwrap();
        check(&g, &login);
        assert!(matches!(
            verify(&request, Mode::Online(&keys), g.now + 3600 + 61),
            Err(Error::Expired)
        ));
        assert!(matches!(
            verify(&request, Mode::Online(&Issuer::get().keys()), g.now),
            Err(Error::UnknownKeyId)
        ));
    }

    fn quiet(result: Result<Login>) -> crate::protocol::Result<()> {
        result.map(|_| ()).map_err(|_| crate::protocol::Error::Size)
    }

    #[test]
    fn mutated_requests_never_panic() {
        let keys = Issuer::get().keys();
        let request = online_request(
            &Issuer::get().token(&claims()),
            &client_data_json(),
            player(),
        );
        let passed = fuzz(&request, 3000, |data| {
            quiet(verify(data, Mode::Online(&keys), NOW))
        });
        assert!(passed > 0);
        let g = goldens();
        let legacy = STD.decode(&g.offline_legacy.request, 1 << 20).unwrap();
        let passed = fuzz(&legacy, 3000, |data| {
            quiet(verify(data, Mode::Offline, g.generated_at))
        });
        assert!(passed > 0);
    }

    /// An ES384 JWT over arbitrary payload bytes.
    fn sign_jwt_bytes(key: &ServerKey, payload: &[u8]) -> String {
        let header = json!({"alg": "ES384", "x5u": key.public_key_base64()}).to_string();
        let mut token = RAW_URL.encode(header.as_bytes());
        token.push('.');
        token.push_str(&RAW_URL.encode(payload));
        let signature = key.sign_es384(token.as_bytes()).unwrap();
        token.push('.');
        token.push_str(&RAW_URL.encode(&signature));
        token
    }

    #[test]
    fn mutated_claims_and_client_data_never_panic() {
        // Mutations behind valid signatures reach the claim and client data parsers.
        let online_token = Issuer::get().token(&claims());
        let data = client_data_json().to_string();
        let passed = fuzz(data.as_bytes(), 4000, |mutated| {
            ClientData::from_json(mutated)
                .map(|_| ())
                .map_err(|_| crate::protocol::Error::Size)
        });
        assert!(passed > 0);
        let raw = sign_jwt(player(), &client_data_json()).unwrap();
        let claims = claims().to_string();
        let header = json!({"alg": "ES384"}).to_string();
        let passed = fuzz(claims.as_bytes(), 4000, |mutated| {
            let token = format!(
                "{}.{}.",
                RAW_URL.encode(header.as_bytes()),
                RAW_URL.encode(mutated)
            );
            let chain = json!({"Token": token}).to_string();
            quiet(verify(&envelope(&chain, &raw), Mode::Offline, NOW))
        });
        assert!(passed > 0);
        // Signing is slow in debug builds, so fewer cases take the online path.
        let keys = Issuer::get().keys();
        let passed = fuzz(claims.as_bytes(), 150, |mutated| {
            let header = json!({"alg": "RS256", "kid": KID}).to_string();
            let token = Issuer::get().sign_bytes(header.as_bytes(), mutated);
            let chain = json!({"Token": token}).to_string();
            quiet(verify(&envelope(&chain, &raw), Mode::Online(&keys), NOW))
        });
        assert!(passed > 0);
        let passed = fuzz(data.as_bytes(), 300, |mutated| {
            let raw = sign_jwt_bytes(player(), mutated);
            let chain = json!({"Token": online_token}).to_string();
            quiet(verify(&envelope(&chain, &raw), Mode::Online(&keys), NOW))
        });
        assert!(passed > 0);
    }
}
