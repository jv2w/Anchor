//! The client data JWT of the Login packet (gophertunnel `login.ClientData`): device, skin and
//! settings. Signed by the client's own key, so every value is the client's claim.

use std::net::Ipv6Addr;

use serde::Deserialize;

use super::{Error, Result, parse_uuid};
use crate::crypto::{
    Algorithm, PublicKey, STD, Verifier, bounded_vec, is_object_or_null, parse_jws, verify_jws,
};

/// Most persona pieces and tint colours read. A persona skin has about 20 pieces.
pub(super) const MAX_PIECES: usize = 128;
/// Most skin animations read. The client sends a few at most.
const MAX_ANIMATIONS: usize = 32;
/// Longest language code read (BCP 47 tags in practice are under 20 characters).
const MAX_LANGUAGE_CODE: usize = 64;
/// Longest host name in a server address (DNS limit).
const MAX_HOST: usize = 253;

/// The client data claims. JSON names are `PascalCase` as the client sends them; a missing field
/// keeps its zero value, as in Go.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(crate) struct ClientData {
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_ANIMATIONS>")]
    pub(crate) animated_image_data: Vec<SkinAnimation>,
    pub(crate) cape_data: String,
    pub(crate) cape_id: String,
    pub(crate) cape_image_height: i64,
    pub(crate) cape_image_width: i64,
    pub(crate) cape_on_classic_skin: bool,
    pub(crate) client_random_id: i64,
    pub(crate) current_input_mode: i64,
    pub(crate) default_input_mode: i64,
    pub(crate) device_model: String,
    #[serde(rename = "DeviceOS")]
    pub(crate) device_os: i32,
    pub(crate) device_id: String,
    pub(crate) game_version: String,
    pub(crate) gui_scale: i64,
    pub(crate) filter_profanity: bool,
    pub(crate) client_editor_connection_intent: i64,
    pub(crate) client_is_editor_capable: bool,
    pub(crate) language_code: String,
    pub(crate) persona_skin: bool,
    pub(crate) platform_offline_id: String,
    pub(crate) platform_online_id: String,
    pub(crate) platform_user_id: String,
    pub(crate) premium_skin: bool,
    pub(crate) self_signed_id: String,
    pub(crate) server_address: String,
    pub(crate) skin_animation_data: String,
    pub(crate) skin_data: String,
    #[serde(rename = "SkinGeometryData")]
    pub(crate) skin_geometry: String,
    #[serde(rename = "SkinGeometryDataEngineVersion")]
    pub(crate) skin_geometry_version: String,
    pub(crate) skin_id: String,
    #[serde(rename = "PlayFabId")]
    pub(crate) playfab_id: String,
    pub(crate) skin_image_height: i64,
    pub(crate) skin_image_width: i64,
    pub(crate) skin_resource_patch: String,
    #[serde(rename = "SkinColor")]
    pub(crate) skin_colour: String,
    pub(crate) arm_size: String,
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_PIECES>")]
    pub(crate) persona_pieces: Vec<PersonaPiece>,
    #[serde(
        rename = "PieceTintColors",
        deserialize_with = "bounded_vec::<_, _, MAX_PIECES>"
    )]
    pub(crate) piece_tint_colours: Vec<PersonaPieceTintColour>,
    pub(crate) third_party_name: String,
    pub(crate) third_party_name_only: Option<bool>,
    #[serde(rename = "UIProfile")]
    pub(crate) ui_profile: i64,
    pub(crate) trusted_skin: bool,
    pub(crate) override_skin: bool,
    pub(crate) compatible_with_client_side_chunk_gen: bool,
    pub(crate) max_view_distance: i64,
    pub(crate) memory_tier: i64,
    pub(crate) platform_type: i64,
    pub(crate) graphics_mode: i64,
    pub(crate) party_id: String,
    #[serde(rename = "IsPartyLeader")]
    pub(crate) party_leader: bool,
    pub(crate) profile_hash: String,
    pub(crate) nonce: String,
}

/// A piece of a persona skin.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(crate) struct PersonaPiece {
    pub(crate) is_default: bool,
    pub(crate) pack_id: String,
    pub(crate) piece_id: String,
    pub(crate) piece_type: String,
    pub(crate) product_id: String,
}

/// Tint colours of a persona piece (ARGB hex).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(crate) struct PersonaPieceTintColour {
    #[serde(rename = "Colors")]
    pub(crate) colours: [String; 4],
    pub(crate) piece_type: String,
}

/// A skin animation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(crate) struct SkinAnimation {
    pub(crate) frames: f64,
    pub(crate) image: String,
    pub(crate) image_height: i64,
    pub(crate) image_width: i64,
    pub(crate) r#type: i64,
    pub(crate) animation_expression: i64,
}

impl ClientData {
    /// Verifies the client data JWT with the client's key and validates the claims.
    pub(crate) fn verify(token: &str, key: &PublicKey) -> Result<Self> {
        let jws = parse_jws(token, token.len()).map_err(|e| Error::Crypto("client data", e))?;
        if jws.header.alg != Algorithm::Es384 {
            return Err(Error::Algorithm("client data"));
        }
        verify_jws(&jws, Verifier::Es384(key)).map_err(|_| Error::Signature("client data"))?;
        Self::from_json(&jws.payload)
    }

    /// Parses and validates the claims JSON.
    pub(super) fn from_json(payload: &[u8]) -> Result<Self> {
        let mut data: Self =
            serde_json::from_slice(payload).map_err(|e| Error::Json("client data", e))?;
        data.bracket_ipv6_address();
        data.validate()?;
        Ok(data)
    }

    /// Clients do not always bracket an IPv6 server address (gophertunnel `Parse`).
    fn bracket_ipv6_address(&mut self) {
        let address = &self.server_address;
        if !address.contains("://")
            && address.matches(':').count() > 1
            && !address.starts_with('[')
            && let Some((host, port)) = address.rsplit_once(':')
        {
            self.server_address = format!("[{host}]:{port}");
        }
    }

    /// gophertunnel `ClientData.Validate`, with its defects fixed: the version pattern applies
    /// to every character, the language code is checked for BCP 47 form without a registry, and
    /// the server address is checked for form instead of being resolved through DNS.
    fn validate(&self) -> Result<()> {
        if !(1..=15).contains(&self.device_os) {
            return Err(Error::ClientData("DeviceOS"));
        }
        if self.game_version.is_empty()
            || !self
                .game_version
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(Error::ClientData("GameVersion"));
        }
        if !is_language_tag(&self.language_code) {
            return Err(Error::ClientData("LanguageCode"));
        }
        if !self.platform_offline_id.is_empty() && parse_uuid(&self.platform_offline_id).is_none() {
            return Err(Error::ClientData("PlatformOfflineId"));
        }
        if !self.platform_online_id.is_empty() && self.platform_online_id.parse::<u64>().is_err() {
            return Err(Error::ClientData("PlatformOnlineId"));
        }
        if !self.self_signed_id.is_empty() && parse_uuid(&self.self_signed_id).is_none() {
            return Err(Error::ClientData("SelfSignedId"));
        }
        if !is_server_address(&self.server_address) {
            return Err(Error::ClientData("ServerAddress"));
        }
        if !image_fits(
            &self.skin_data,
            self.skin_image_height,
            self.skin_image_width,
        ) {
            return Err(Error::ClientData("SkinData"));
        }
        if !image_fits(
            &self.cape_data,
            self.cape_image_height,
            self.cape_image_width,
        ) {
            return Err(Error::ClientData("CapeData"));
        }
        if !self
            .playfab_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::ClientData("PlayFabId"));
        }
        for animation in &self.animated_image_data {
            if !image_fits(
                &animation.image,
                animation.image_height,
                animation.image_width,
            ) || !(0..=3).contains(&animation.r#type)
            {
                return Err(Error::ClientData("AnimatedImageData"));
            }
        }
        let geometry = STD
            .decode(&self.skin_geometry, self.skin_geometry.len())
            .map_err(|_| Error::ClientData("SkinGeometryData"))?;
        if !geometry.is_empty() && !is_object_or_null(&geometry) {
            return Err(Error::ClientData("SkinGeometryData"));
        }
        let patch = STD
            .decode(&self.skin_resource_patch, self.skin_resource_patch.len())
            .map_err(|_| Error::ClientData("SkinResourcePatch"))?;
        if !is_object_or_null(&patch) {
            return Err(Error::ClientData("SkinResourcePatch"));
        }
        if self.skin_id.is_empty() {
            return Err(Error::ClientData("SkinId"));
        }
        if !(0..=2).contains(&self.ui_profile) {
            return Err(Error::ClientData("UIProfile"));
        }
        Ok(())
    }
}

/// Whether base64 `image` decodes to exactly `height * width * 4` bytes (RGBA).
fn image_fits(image: &str, height: i64, width: i64) -> bool {
    let expected = height
        .checked_mul(width)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| usize::try_from(n).ok());
    expected.is_some_and(|n| STD.decoded_len(image).is_ok_and(|len| len == n))
}

/// Whether `code` (`en_US`) is a well-formed BCP 47 tag with `_` or `-` separators: a 2 to 8
/// letter language, then subtags of 1 to 8 letters or digits. gophertunnel also looks the tag up
/// in the IANA registry; the value is informative only, so the form is enough here.
fn is_language_tag(code: &str) -> bool {
    if code.len() > MAX_LANGUAGE_CODE {
        return false;
    }
    let mut subtags = code.split(['_', '-']);
    let language_ok = subtags
        .next()
        .is_some_and(|l| (2..=8).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphabetic()));
    language_ok
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Whether `address` is `host:port`, `[ipv6]:port`, or the NetherNet form
/// `http(s)://host:port:port` with both ports equal (gophertunnel `ClientData.Validate`).
fn is_server_address(address: &str) -> bool {
    if let Some((scheme, _)) = address.split_once("://") {
        // The last `:port` is NetherNet's; the URL before it must carry the same port.
        let Some((url, port)) = address.rsplit_once(':') else {
            return false;
        };
        let Some((_, rest)) = url.split_once("://") else {
            return false;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let url_port = match authority.rsplit_once(':') {
            Some((host, p)) if !host.contains('[') || host.ends_with(']') => p,
            _ => "",
        };
        return matches!(scheme, "http" | "https")
            && !authority.is_empty()
            && is_port(url_port)
            && url_port == port;
    }
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    if !is_port(port) {
        return false;
    }
    if let Some(inner) = host.strip_prefix('[') {
        let Some(inner) = inner.strip_suffix(']') else {
            return false;
        };
        let ip = inner.split_once('%').map_or(inner, |(ip, _zone)| ip);
        return ip.parse::<Ipv6Addr>().is_ok();
    }
    host.len() <= MAX_HOST
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn is_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}
