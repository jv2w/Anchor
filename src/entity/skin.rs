//! The look of a human NPC: an image on a geometry, checked when it is made, so a skin from the
//! user or from a saved chunk is always one a client can draw.

use std::fmt;

use crate::{crypto, protocol};

/// The size of a cape image: [`CAPE_WIDTH`] × [`CAPE_HEIGHT`] pixels of RGBA.
const CAPE_BYTES: usize = CAPE_WIDTH as usize * CAPE_HEIGHT as usize * 4;
/// The height of a cape image, in pixels.
const CAPE_HEIGHT: u32 = 32;
/// The width of a cape image, in pixels.
const CAPE_WIDTH: u32 = 64;
/// The geometry of the standard player model.
const DEFAULT_GEOMETRY: &str = "geometry.humanoid.custom";
/// The largest geometry JSON, in bytes (vanilla geometries are a few KiB).
const MAX_GEOMETRY_BYTES: usize = 1 << 20;
/// The longest skin ID and geometry name, in bytes.
const MAX_NAME_BYTES: usize = 256;
/// The most bytes any skin takes on the wire (images, geometry, resource patch, animation data,
/// texts and the fixed fields of its animations and persona pieces): what a login accepts, an
/// in-game change and the API may set. A persona skin with its geometry and animations takes well
/// under 1 MiB; the cap bounds what every other player is sent when the player joins
/// (`PlayerList`) or changes it, and `max-batch-bytes` is at least one player list entry with
/// such a skin.
pub const MAX_SKIN_BYTES: usize = 4 << 20;
/// The deepest nesting of JSON objects and arrays in a skin's geometry and resource patch: the
/// protocol requires both to be depth-limited JSON, and the official server refuses deeper skin
/// JSON from clients.
pub(crate) const MAX_SKIN_JSON_DEPTH: usize = 64;
/// The geometry JSON sent for a vanilla geometry: the field must hold JSON, and an empty one
/// declares no custom geometry.
pub(crate) const NO_CUSTOM_GEOMETRY: &[u8] = b"{}";

/// A skin: an ID, an RGBA image of an accepted size, an optional cape and the geometry it is drawn
/// on (a vanilla geometry name, or a custom one with its JSON).
///
/// It is the skin of a human NPC, given when the NPC is spawned. A player's skin is the
/// protocol's skin that [`Player::skin`](crate::player::Player::skin) lends.
#[derive(Clone, PartialEq, Eq, Hash)]
// api-class: Skin
pub struct Skin {
    id: String,
    data: Vec<u8>,
    cape: Vec<u8>,
    geometry_name: String,
    geometry_data: Vec<u8>,
}

impl Skin {
    /// The image sizes a skin may have, each as its bytes of RGBA, width and height in pixels:
    /// 64 × 32, 64 × 64 and 128 × 128.
    #[doc(alias = "Skin::ACCEPTED_SKIN_SIZES")]
    pub const SIZES: [(usize, u32, u32); 3] = [
        (64 * 32 * 4, 64, 32),
        (64 * 64 * 4, 64, 64),
        (128 * 128 * 4, 128, 128),
    ];

    /// A skin with the image `data` (RGBA rows of 64 × 32, 64 × 64 or 128 × 128 pixels) on the
    /// standard player model, without a cape. `None` if `id` is empty or longer than 256 bytes or
    /// `data` has another size.
    #[doc(alias = "Skin::__construct")]
    pub fn new(id: impl Into<String>, data: Vec<u8>) -> Option<Self> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_NAME_BYTES || dimensions(data.len()).is_none() {
            return None;
        }
        Some(Self {
            id,
            data,
            cape: Vec::new(),
            geometry_name: DEFAULT_GEOMETRY.to_owned(),
            geometry_data: Vec::new(),
        })
    }

    /// The RGBA cape image, empty without a cape.
    #[doc(alias = "Skin::getCapeData")]
    pub fn cape(&self) -> &[u8] {
        &self.cape
    }

    /// The RGBA image.
    #[doc(alias = "Skin::getSkinData")]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The custom geometry JSON, empty for a vanilla geometry.
    #[doc(alias = "Skin::getGeometryData")]
    pub fn geometry_data(&self) -> &[u8] {
        &self.geometry_data
    }

    /// The geometry name.
    #[doc(alias = "Skin::getGeometryName")]
    pub fn geometry_name(&self) -> &str {
        &self.geometry_name
    }

    /// The skin ID.
    #[doc(alias = "Skin::getSkinId")]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The same skin with a cape image of 64 × 32 RGBA pixels, or without one for an empty
    /// `cape`. `None` for another size.
    pub fn with_cape(self, cape: Vec<u8>) -> Option<Self> {
        (cape.is_empty() || cape.len() == CAPE_BYTES).then_some(Self { cape, ..self })
    }

    /// The same skin drawn on the geometry `name` (such as `geometry.humanoid.customSlim`),
    /// defined by the JSON `data` when it is a custom one (empty for a vanilla geometry). `None`
    /// if `name` is empty, over 256 bytes or not made of ASCII letters, digits and `._:-`, or
    /// `data` is over 1 MiB, not JSON or nested more than 64 objects and arrays deep.
    pub fn with_geometry(self, name: impl Into<String>, data: Vec<u8>) -> Option<Self> {
        let name = name.into();
        let valid_name = !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
        let valid_data = data.is_empty()
            || (data.len() <= MAX_GEOMETRY_BYTES
                && crypto::is_json_within_depth(&data, MAX_SKIN_JSON_DEPTH));
        (valid_name && valid_data).then_some(Self {
            geometry_name: name,
            geometry_data: data,
            ..self
        })
    }

    /// The skin as the protocol carries it (the player list and `PlayerSkin`).
    pub(crate) fn to_protocol(&self) -> protocol::Skin {
        #[expect(
            clippy::expect_used,
            reason = "invariant: `new` checked the image size and nothing else sets `data`"
        )]
        let (_, width, height) =
            dimensions(self.data.len()).expect("the image has an accepted size");
        // The name is checked to need no JSON escaping.
        let patch = format!(r#"{{"geometry":{{"default":"{}"}}}}"#, self.geometry_name);
        let has_cape = !self.cape.is_empty();
        protocol::Skin {
            skin_id: self.id.clone(),
            full_id: self.id.clone(),
            skin_resource_patch: patch.into_bytes(),
            skin_image_width: width,
            skin_image_height: height,
            skin_data: self.data.clone(),
            cape_image_width: if has_cape { CAPE_WIDTH } else { 0 },
            cape_image_height: if has_cape { CAPE_HEIGHT } else { 0 },
            cape_data: self.cape.clone(),
            skin_geometry: if self.geometry_data.is_empty() {
                NO_CUSTOM_GEOMETRY.to_vec()
            } else {
                self.geometry_data.clone()
            },
            geometry_data_engine_version: b"0.0.0".to_vec(),
            arm_size: protocol::consts::ArmSize::Wide.into(),
            trusted: true,
            ..protocol::Skin::default()
        }
    }
}

impl fmt::Debug for Skin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Skin")
            .field("id", &self.id)
            .field("data", &self.data.len())
            .field("cape", &self.cape.len())
            .field("geometry_name", &self.geometry_name)
            .field("geometry_data", &self.geometry_data.len())
            .finish()
    }
}

/// The accepted size an image of `len` bytes has, with its width and height.
fn dimensions(len: usize) -> Option<(usize, u32, u32)> {
    Skin::SIZES.into_iter().find(|&(size, _, _)| size == len)
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: Skin::__construct, Skin::getSkinId, Skin::getSkinData, Skin::getCapeData,
    // api: Skin::getGeometryName, Skin::getGeometryData, Skin
    #[test]
    fn skins_take_only_accepted_sizes_and_safe_geometry_names() {
        for (size, w, h) in Skin::SIZES {
            let skin = Skin::new("npc", vec![7; size]).unwrap();
            let p = skin.to_protocol();
            assert_eq!((p.skin_image_width, p.skin_image_height), (w, h));
            assert_eq!(skin.data().len(), size);
        }
        assert!(Skin::new("npc", vec![0; 100]).is_none());
        assert!(Skin::new("", vec![0; 8192]).is_none());
        let skin = Skin::new("npc", vec![0; 8192]).unwrap();
        assert_eq!(skin.id(), "npc");
        assert_eq!(skin.geometry_name(), DEFAULT_GEOMETRY);
        assert!(skin.cape().is_empty() && skin.geometry_data().is_empty());
        assert!(skin.clone().with_cape(vec![0; 10]).is_none());
        let caped = skin.clone().with_cape(vec![1; CAPE_BYTES]).unwrap();
        assert_eq!(caped.to_protocol().cape_image_width, 64);
        assert!(skin.clone().with_geometry("a\"b", Vec::new()).is_none());
        assert!(skin.clone().with_geometry("geo.x", b"{".to_vec()).is_none());
        let slim = skin
            .with_geometry("geometry.humanoid.customSlim", b"{}".to_vec())
            .unwrap();
        assert_eq!(slim.geometry_data(), b"{}");
        assert_eq!(
            slim.to_protocol().skin_resource_patch,
            br#"{"geometry":{"default":"geometry.humanoid.customSlim"}}"#
        );
        assert!(format!("{slim:?}").contains("8192"));
    }

    #[test]
    fn skins_carry_geometry_json_a_login_accepts() {
        let skin = Skin::new("npc", vec![0; 8192]).unwrap();
        let vanilla = skin.to_protocol();
        assert_eq!(vanilla.skin_geometry, NO_CUSTOM_GEOMETRY);
        let accepted = |s: protocol::Skin| crate::tick::queue::Skin::new(s.into()).is_acceptable();
        assert!(accepted(vanilla));
        // An object `n` deep.
        let nested = |n: usize| {
            format!("{}{{}}{}", r#"{"a":"#.repeat(n - 1), "}".repeat(n - 1)).into_bytes()
        };
        let custom = skin
            .clone()
            .with_geometry("geo.x", nested(MAX_SKIN_JSON_DEPTH))
            .unwrap();
        assert!(accepted(custom.to_protocol()));
        assert!(
            skin.with_geometry("geo.x", nested(MAX_SKIN_JSON_DEPTH + 1))
                .is_none()
        );
    }
}
