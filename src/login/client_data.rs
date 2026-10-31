//! The client data JWT of the Login packet (gophertunnel `login.ClientData`): device, skin and
//! settings. Signed by the client's own key, so every value is the client's claim.

use std::net::Ipv6Addr;
use std::sync::Arc;

use serde::Deserialize;

use super::{Error, Result, parse_uuid};
use crate::crypto::{
    Algorithm, PublicKey, STD, Verifier, bounded_vec, is_object_or_null, parse_jws, verify_jws,
};
use crate::entity::{MAX_SKIN_BYTES, NO_CUSTOM_GEOMETRY};
use crate::math::to_f32;
use crate::protocol::consts::{ArmSize, PieceType};
use crate::protocol::{self, Rgba};
use crate::tick::queue::Skin;

/// Most persona pieces and tint colours read. A persona skin has about 20 pieces.
pub(super) const MAX_PIECES: usize = 128;
/// Most skin animations read. The client sends a few at most.
const MAX_ANIMATIONS: usize = 32;
/// Longest language code read (BCP 47 tags in practice are under 20 characters).
const MAX_LANGUAGE_CODE: usize = 64;
/// Longest host name in a server address (DNS limit).
const MAX_HOST: usize = 253;
/// The persona piece type names of the login data and their protocol types; any other name is
/// [`PieceType::Unknown`].
const PIECE_TYPES: [(&str, PieceType); 28] = [
    ("persona_skeleton", PieceType::Skeleton),
    ("persona_body", PieceType::Body),
    ("persona_skin", PieceType::Skin),
    ("persona_bottom", PieceType::Bottom),
    ("persona_feet", PieceType::Feet),
    ("persona_dress", PieceType::Dress),
    ("persona_top", PieceType::Top),
    ("persona_high_pants", PieceType::HighPants),
    ("persona_hand", PieceType::Hands),
    ("persona_outerwear", PieceType::Outerwear),
    ("persona_facial_hair", PieceType::FacialHair),
    ("persona_mouth", PieceType::Mouth),
    ("persona_eyes", PieceType::Eyes),
    ("persona_hair", PieceType::Hair),
    ("persona_hood", PieceType::Hood),
    ("persona_back", PieceType::Back),
    ("persona_face_accessory", PieceType::FaceAccessory),
    ("persona_head", PieceType::Head),
    ("persona_legs", PieceType::Legs),
    ("persona_left_leg", PieceType::LeftLeg),
    ("persona_right_leg", PieceType::RightLeg),
    ("persona_arms", PieceType::Arms),
    ("persona_left_arm", PieceType::LeftArm),
    ("persona_right_arm", PieceType::RightArm),
    ("persona_capes", PieceType::Capes),
    ("persona_classic_skin", PieceType::ClassicSkin),
    ("persona_emote", PieceType::Emote),
    ("persona_coco", PieceType::CoCo),
];
/// The login tint piece types whose protocol name is not the login name without `persona_`: the
/// protocol names a tint's piece by the `persona::PieceType` name in lower case (Mojang protocol
/// docs, the Cloudburst protocol library), which drops the underscore of these. gophertunnel only drops `persona_`,
/// so its codec would send `facial_hair`, a name the client does not read.
const TINT_PROTOCOL_NAMES: [(&str, &str); 7] = [
    ("persona_facial_hair", "facialhair"),
    ("persona_face_accessory", "faceaccessory"),
    ("persona_left_leg", "leftleg"),
    ("persona_right_leg", "rightleg"),
    ("persona_left_arm", "leftarm"),
    ("persona_right_arm", "rightarm"),
    ("persona_classic_skin", "classicskin"),
];

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
    /// The skin the client sent, as the protocol carries it to other players (PMMP
    /// `ClientDataToSkinDataHelper::fromClientData`, without PMMP's `LegacySkinAdapter`, which
    /// swaps persona skins for a random colour: Anchor keeps the skin as sent). The images,
    /// geometry (`{}` when empty), resource patch and animation data are decoded from base64, tint
    /// piece types take their protocol names ([`TINT_PROTOCOL_NAMES`]); the full skin ID is
    /// the skin ID and the cape ID together (PMMP draws a random UUID; BDS's value is not
    /// confirmed). Fails if a field does not decode, an image dimension or animation number is
    /// out of range, or the skin is not acceptable ([`Skin::is_acceptable`]).
    pub(crate) fn skin(&self) -> Result<Skin> {
        // Decoding allocates at most the bound; the texts are counted with the skin after.
        let mut budget = MAX_SKIN_BYTES;
        let mut decode = |text: &str| {
            let bytes = STD
                .decode(text, budget)
                .map_err(|_| Error::ClientData("Skin"))?;
            budget = budget.saturating_sub(bytes.len());
            Ok::<_, Error>(bytes)
        };
        let dimension = |n: i64| u32::try_from(n).map_err(|_| Error::ClientData("Skin"));
        let mut animations = Vec::with_capacity(self.animated_image_data.len());
        for a in &self.animated_image_data {
            animations.push(protocol::SkinAnimation {
                image_width: dimension(a.image_width)?,
                image_height: dimension(a.image_height)?,
                image_data: decode(&a.image)?,
                animation_type: dimension(a.r#type)?,
                frame_count: to_f32(a.frames),
                expression_type: dimension(a.animation_expression)?,
            });
        }
        let skin = Skin::new(Arc::new(protocol::Skin {
            skin_id: self.skin_id.clone(),
            skin_resource_patch: decode(&self.skin_resource_patch)?,
            skin_image_width: dimension(self.skin_image_width)?,
            skin_image_height: dimension(self.skin_image_height)?,
            skin_data: decode(&self.skin_data)?,
            animations,
            cape_image_width: dimension(self.cape_image_width)?,
            cape_image_height: dimension(self.cape_image_height)?,
            cape_data: decode(&self.cape_data)?,
            // The login may leave the geometry empty for none; the protocol carries none as `{}`.
            skin_geometry: match decode(&self.skin_geometry)? {
                geometry if geometry.is_empty() => NO_CUSTOM_GEOMETRY.to_vec(),
                geometry => geometry,
            },
            animation_data: decode(&self.skin_animation_data)?,
            geometry_data_engine_version: decode(&self.skin_geometry_version)?,
            premium_skin: self.premium_skin,
            persona_skin: self.persona_skin,
            persona_cape_on_classic_skin: self.cape_on_classic_skin,
            // PMMP: the client data has no field for it.
            primary_user: true,
            cape_id: self.cape_id.clone(),
            full_id: format!("{}{}", self.skin_id, self.cape_id),
            skin_colour: colour(&self.skin_colour),
            arm_size: arm_size(&self.arm_size).into(),
            persona_pieces: self
                .persona_pieces
                .iter()
                .map(|p| protocol::PersonaPiece {
                    piece_id: p.piece_id.clone(),
                    piece_type: piece_type(&p.piece_type),
                    pack_id: parse_uuid(&p.pack_id).unwrap_or_default(),
                    default: p.is_default,
                    product_id: p.product_id.clone(),
                })
                .collect(),
            piece_tint_colours: self
                .piece_tint_colours
                .iter()
                .map(|t| protocol::PersonaPieceTintColour {
                    piece_type: tint_piece_type(&t.piece_type),
                    colours: t.colours.each_ref().map(|c| colour(c)),
                })
                .collect(),
            trusted: self.trusted_skin,
            override_appearance: self.override_skin,
            profile_hash: self.profile_hash.clone(),
        }));
        if !skin.is_acceptable() {
            return Err(Error::ClientData("Skin"));
        }
        Ok(skin)
    }

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

/// The protocol's number of a login persona piece type (the login data says `persona_hand`,
/// the wire `hands`); 0 (unknown) for any other name.
fn piece_type(name: &str) -> u32 {
    let name = if name == "persona_hands" {
        "persona_hand"
    } else {
        name
    };
    let piece_type = PIECE_TYPES
        .iter()
        .find(|(t, _)| *t == name)
        .map_or(PieceType::Unknown, |&(_, piece_type)| piece_type);
    u32::from(piece_type)
}

/// The protocol value of a login tint piece type: the codec sends the value without `persona_`
/// (and `persona_hand` as `hands`), so the types whose protocol name differs
/// ([`TINT_PROTOCOL_NAMES`]) take the value the codec reads from that name.
fn tint_piece_type(name: &str) -> String {
    TINT_PROTOCOL_NAMES
        .iter()
        .find(|(login, _)| *login == name)
        .map_or_else(
            || name.to_owned(),
            |(_, wire)| protocol::persona_piece_tint_login_type(wire),
        )
}

/// The arm size the login data names: slim, or wide for anything else.
fn arm_size(name: &str) -> ArmSize {
    if name == "slim" {
        ArmSize::Slim
    } else {
        ArmSize::Wide
    }
}

/// A colour of the login data: `#AARRGGBB`, or `#RRGGBB` (opaque) as the skin colour is sent
/// (gophertunnel: "#b37b62"); anything else is transparent black.
fn colour(text: &str) -> Rgba {
    let clear = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    let Some(hex) = text.strip_prefix('#') else {
        return clear;
    };
    if !(1..=8).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return clear;
    }
    let Ok(argb) = u32::from_str_radix(hex, 16) else {
        return clear;
    };
    let [alpha, red, green, blue] = argb.to_be_bytes();
    Rgba {
        r: red,
        g: green,
        b: blue,
        a: if hex.len() > 6 { alpha } else { u8::MAX },
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

/// Whether `address` is `host:port`, `[ipv6]:port`, or a NetherNet `http(s)://host:port` URL,
/// which clients before 1.26.60 follow with the same `:port` again (gophertunnel
/// `ClientData.Validate` after v1.63.0).
fn is_server_address(address: &str) -> bool {
    if let Some((scheme, rest)) = address.split_once("://") {
        let url = match rest.rsplit_once(':') {
            Some((url, port)) if url_host_port(url).is_some_and(|(_, p)| p == port) => url,
            _ => rest,
        };
        return matches!(scheme, "http" | "https") && url_host_port(url).is_some();
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

/// The host and port of the authority of `url` (a URL after its `scheme://`) when it has both:
/// `host:port` or `[ipv6]:port`.
fn url_host_port(url: &str) -> Option<(&str, &str)> {
    let authority = url.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = authority.rsplit_once(':')?;
    let host_ok = match host.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']').is_some_and(|ip| !ip.is_empty()),
        None => !host.is_empty() && !host.contains([':', ']']),
    };
    (host_ok && is_port(port)).then_some((host, port))
}

fn is_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}
