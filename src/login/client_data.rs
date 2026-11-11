//! The client data JWT of the Login packet (gophertunnel `login.ClientData`): device, skin and
//! settings. Signed by the client's own key, so every value is the client's claim.

use std::net::Ipv6Addr;
use std::sync::Arc;

use serde::Deserialize;

use super::{Error, Result};
use crate::crypto::{Algorithm, PublicKey, STD, Verifier, bounded_vec, parse_jws, verify_jws};
use crate::entity::{MAX_SKIN_BYTES, NO_CUSTOM_GEOMETRY};
use crate::math::to_f32;
use crate::protocol::consts::{ArmSize, DeviceOs, PieceType};
use crate::protocol::{self, Rgba};
use crate::tick::queue::Skin;

/// Most skin animations read. The client sends a few at most.
const MAX_ANIMATIONS: usize = 32;
/// Longest `DeviceId` and `DeviceModel`, in bytes. The player keeps both for its whole stay;
/// real values are under 64 bytes.
const MAX_DEVICE_TEXT_BYTES: usize = 256;
/// Longest `GameVersion`, in bytes. The player keeps it; a real one is `1.26.50`.
const MAX_GAME_VERSION_BYTES: usize = 32;
/// Longest host name in a server address (DNS limit).
const MAX_HOST_BYTES: usize = 253;
/// Longest language code read (BCP 47 tags in practice are under 20 characters).
const MAX_LANGUAGE_CODE_BYTES: usize = 64;
/// Most persona pieces and tint colors read. A persona skin has about 20 pieces.
pub(super) const MAX_PIECES: usize = 128;
/// Longest `ServerAddress`, in bytes. The player keeps it: a DNS host name ([`MAX_HOST_BYTES`]),
/// a scheme, two ports and a short path.
const MAX_SERVER_ADDRESS_BYTES: usize = 512;
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
/// docs), which drops the underscore of these. gophertunnel
/// only drops `persona_`, so its codec would send `facial_hair`, a name the client does not read.
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
    animated_image_data: Vec<SkinAnimation>,
    cape_data: String,
    cape_id: String,
    cape_image_height: i64,
    cape_image_width: i64,
    cape_on_classic_skin: bool,
    pub(crate) client_random_id: i64,
    pub(crate) current_input_mode: i64,
    default_input_mode: i64,
    pub(crate) device_model: String,
    #[serde(rename = "DeviceOS")]
    pub(crate) device_os: i32,
    pub(crate) device_id: String,
    pub(crate) game_version: String,
    gui_scale: i64,
    filter_profanity: bool,
    client_editor_connection_intent: i64,
    client_is_editor_capable: bool,
    pub(crate) language_code: String,
    persona_skin: bool,
    platform_offline_id: String,
    platform_online_id: String,
    platform_user_id: String,
    premium_skin: bool,
    pub(crate) self_signed_id: String,
    pub(crate) server_address: String,
    skin_animation_data: String,
    skin_data: String,
    #[serde(rename = "SkinGeometryData")]
    skin_geometry: String,
    #[serde(rename = "SkinGeometryDataEngineVersion")]
    skin_geometry_version: String,
    pub(super) skin_id: String,
    #[serde(rename = "PlayFabId")]
    playfab_id: String,
    skin_image_height: i64,
    skin_image_width: i64,
    skin_resource_patch: String,
    #[serde(rename = "SkinColor")]
    skin_color: String,
    arm_size: String,
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_PIECES>")]
    persona_pieces: Vec<PersonaPiece>,
    #[serde(
        rename = "PieceTintColors",
        deserialize_with = "bounded_vec::<_, _, MAX_PIECES>"
    )]
    piece_tint_colors: Vec<PersonaPieceTintColor>,
    third_party_name: String,
    third_party_name_only: Option<bool>,
    #[serde(rename = "UIProfile")]
    ui_profile: i64,
    trusted_skin: bool,
    override_skin: bool,
    compatible_with_client_side_chunk_gen: bool,
    max_view_distance: i64,
    memory_tier: i64,
    platform_type: i64,
    graphics_mode: i64,
    party_id: String,
    #[serde(rename = "IsPartyLeader")]
    party_leader: bool,
    profile_hash: String,
    nonce: String,
}

impl ClientData {
    /// Parses and validates the claims JSON.
    pub(super) fn from_json(payload: &[u8]) -> Result<Self> {
        let mut data: Self = serde_json::from_slice(payload).map_err(|source| Error::Json {
            part: "client data",
            source,
        })?;
        data.bracket_ipv6_address();
        data.validate()?;
        Ok(data)
    }

    /// Converts the skin the client sent to the form the protocol carries to other players.
    /// The images, geometry (`{}` when empty), resource patch and animation data are decoded from
    /// base64, tint piece types take their protocol names ([`TINT_PROTOCOL_NAMES`]), and the full
    /// skin ID is the skin ID and the cape ID together. Fails if a field does not decode, an image
    /// dimension or animation number is out of range, or the skin is not acceptable
    /// ([`Skin::is_acceptable`], which holds the geometry and resource patch to a JSON object or
    /// null as gophertunnel `ClientData.Validate` does).
    pub(super) fn skin(&self) -> Result<Skin> {
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
        let skin_resource_patch = decode(&self.skin_resource_patch)?;
        // The login may leave the geometry empty for none; the protocol carries none as `{}`.
        let skin_geometry = match decode(&self.skin_geometry)? {
            geometry if geometry.is_empty() => NO_CUSTOM_GEOMETRY.to_vec(),
            geometry => geometry,
        };
        let skin = Skin::new(Arc::new(protocol::Skin {
            skin_id: self.skin_id.clone(),
            skin_resource_patch,
            skin_image_width: dimension(self.skin_image_width)?,
            skin_image_height: dimension(self.skin_image_height)?,
            skin_data: decode(&self.skin_data)?,
            animations,
            cape_image_width: dimension(self.cape_image_width)?,
            cape_image_height: dimension(self.cape_image_height)?,
            cape_data: decode(&self.cape_data)?,
            skin_geometry,
            animation_data: decode(&self.skin_animation_data)?,
            geometry_data_engine_version: decode(&self.skin_geometry_version)?,
            premium_skin: self.premium_skin,
            persona_skin: self.persona_skin,
            persona_cape_on_classic_skin: self.cape_on_classic_skin,
            // The client data has no field for it.
            primary_user: true,
            cape_id: self.cape_id.clone(),
            full_id: format!("{}{}", self.skin_id, self.cape_id),
            skin_colour: color(&self.skin_color),
            arm_size: arm_size(&self.arm_size).into(),
            persona_pieces: self
                .persona_pieces
                .iter()
                .map(|p| protocol::PersonaPiece {
                    piece_id: p.piece_id.clone(),
                    piece_type: piece_type(&p.piece_type),
                    pack_id: p.pack_id.parse().unwrap_or_default(),
                    default: p.is_default,
                    product_id: p.product_id.clone(),
                })
                .collect(),
            piece_tint_colours: self
                .piece_tint_colors
                .iter()
                .map(|t| protocol::PersonaPieceTintColour {
                    piece_type: tint_piece_type(&t.piece_type),
                    colours: t.colors.each_ref().map(|c| color(c)),
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
    pub(super) fn verify(token: &str, key: &PublicKey) -> Result<Self> {
        let jws = parse_jws(token, token.len()).map_err(|source| Error::Crypto {
            part: "client data",
            source,
        })?;
        if jws.header.alg != Algorithm::Es384 {
            return Err(Error::Algorithm("client data"));
        }
        verify_jws(&jws, Verifier::Es384(key)).map_err(|_| Error::Signature("client data"))?;
        Self::from_json(&jws.payload)
    }

    /// Checks the claims as gophertunnel `ClientData.Validate` does, with its defects fixed: the
    /// version pattern applies to every character, the language code is checked for BCP 47 form
    /// without a registry, and the server address is checked for form instead of being resolved
    /// through DNS. The geometry and the resource patch are checked where [`skin`](Self::skin)
    /// decodes them.
    fn validate(&self) -> Result<()> {
        if !DeviceOs::try_from(self.device_os).is_ok_and(|os| os != DeviceOs::Unknown) {
            return Err(Error::ClientData("DeviceOS"));
        }
        if self.device_id.len() > MAX_DEVICE_TEXT_BYTES {
            return Err(Error::ClientData("DeviceId"));
        }
        if self.device_model.len() > MAX_DEVICE_TEXT_BYTES {
            return Err(Error::ClientData("DeviceModel"));
        }
        if self.game_version.is_empty()
            || self.game_version.len() > MAX_GAME_VERSION_BYTES
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
        if !self.platform_offline_id.is_empty()
            && self.platform_offline_id.parse::<protocol::Uuid>().is_err()
        {
            return Err(Error::ClientData("PlatformOfflineId"));
        }
        if !self.platform_online_id.is_empty() && self.platform_online_id.parse::<u64>().is_err() {
            return Err(Error::ClientData("PlatformOnlineId"));
        }
        if !self.self_signed_id.is_empty() && self.self_signed_id.parse::<protocol::Uuid>().is_err()
        {
            return Err(Error::ClientData("SelfSignedId"));
        }
        if self.server_address.len() > MAX_SERVER_ADDRESS_BYTES
            || !is_server_address(&self.server_address)
        {
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
        if self.skin_id.is_empty() {
            return Err(Error::ClientData("SkinId"));
        }
        if !(0..=2).contains(&self.ui_profile) {
            return Err(Error::ClientData("UIProfile"));
        }
        Ok(())
    }

    /// Brackets an IPv6 server address the client left bare (gophertunnel `Parse`).
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
}

/// A piece of a persona skin.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct PersonaPiece {
    is_default: bool,
    pack_id: String,
    piece_id: String,
    piece_type: String,
    product_id: String,
}

/// Tint colors of a persona piece (ARGB hex).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct PersonaPieceTintColor {
    #[serde(rename = "Colors")]
    colors: [String; 4],
    piece_type: String,
}

/// A skin animation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct SkinAnimation {
    frames: f64,
    image: String,
    image_height: i64,
    image_width: i64,
    r#type: i64,
    animation_expression: i64,
}

/// The protocol's number of a login persona piece type (the login data says
/// `persona_hand`, the wire `hands`); 0 (unknown) for any other name.
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

/// The protocol value of a login tint piece type: the codec sends the value without
/// `persona_` (and `persona_hand` as `hands`), so the types whose protocol name differs
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

/// Reads a color of the login data: `#AARRGGBB`, or `#RRGGBB` (opaque) as the skin color is
/// sent (gophertunnel: "#b37b62"); anything else is transparent black.
fn color(text: &str) -> Rgba {
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
    if code.len() > MAX_LANGUAGE_CODE_BYTES {
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
    host.len() <= MAX_HOST_BYTES
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Splits the authority of `url` (a URL after its `scheme://`) into its host and port when it
/// has both: `host:port` or `[ipv6]:port`.
fn url_host_port(url: &str) -> Option<(&str, &str)> {
    let authority = url
        .split_once(['/', '?', '#'])
        .map_or(url, |(authority, _)| authority);
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
