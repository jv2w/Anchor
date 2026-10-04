//! The look of a human NPC (PMMP `Skin`): an image on a geometry, checked when it is made, so a
//! skin from the user or from a saved chunk is always one a client can draw.

use crate::protocol;

/// The image sizes a skin may have, in bytes of RGBA (PMMP `Skin::ACCEPTED_SKIN_SIZES`): 64 × 32,
/// 64 × 64 and 128 × 128 pixels.
const SIZES: [(usize, u32, u32); 3] = [
    (64 * 32 * 4, 64, 32),
    (64 * 64 * 4, 64, 64),
    (128 * 128 * 4, 128, 128),
];
/// The size of a cape image: 64 × 32 pixels of RGBA.
const CAPE_SIZE: usize = 64 * 32 * 4;
/// The longest skin ID and geometry name, in bytes.
const MAX_NAME_BYTES: usize = 256;
/// The largest geometry JSON, in bytes (vanilla geometries are a few KiB).
const MAX_GEOMETRY_BYTES: usize = 1 << 20;
/// The most bytes any skin holds in all (images, geometry, resource patch, animation data),
/// decoded: what a login accepts, an in-game change and the API may set. A persona skin with its
/// geometry and animations takes well under 1 MiB; the cap bounds what every other player is
/// sent when the player joins (`PlayerList`) or changes it, whatever batch size the server
/// accepts.
pub(crate) const MAX_SKIN_BYTES: usize = 4 << 20;
/// The geometry of the standard player model.
const DEFAULT_GEOMETRY: &str = "geometry.humanoid.custom";

/// A skin: an ID, an RGBA image of an accepted size, an optional cape and the geometry it is drawn
/// on (a vanilla geometry name, or a custom one with its JSON).
#[derive(Clone, PartialEq, Eq, Hash)]
// pmmp-class: Skin
pub struct Skin {
    id: String,
    data: Vec<u8>,
    cape: Vec<u8>,
    geometry_name: String,
    geometry_data: Vec<u8>,
}

impl Skin {
    /// A skin with the image `data` (RGBA rows of 64 × 32, 64 × 64 or 128 × 128 pixels) on the
    /// standard player model, without a cape. `None` if `id` is empty or longer than 256 bytes or
    /// `data` has another size (PMMP throws).
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

    /// This skin with a cape image of 64 × 32 RGBA pixels, or without one for an empty `cape`.
    /// `None` for another size.
    pub fn with_cape(self, cape: Vec<u8>) -> Option<Self> {
        (cape.is_empty() || cape.len() == CAPE_SIZE).then_some(Self { cape, ..self })
    }

    /// This skin drawn on the geometry `name` (such as `geometry.humanoid.customSlim`), defined by
    /// the JSON `data` when it is a custom one (empty for a vanilla geometry). `None` if `name` is
    /// empty, over 256 bytes or not made of ASCII letters, digits and `._:-`, or `data` is over
    /// 1 MiB or not JSON.
    pub fn with_geometry(self, name: impl Into<String>, data: Vec<u8>) -> Option<Self> {
        let name = name.into();
        let valid_name = !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
        let valid_data = data.is_empty()
            || (data.len() <= MAX_GEOMETRY_BYTES
                && serde_json::from_slice::<serde_json::Value>(&data).is_ok());
        (valid_name && valid_data).then_some(Self {
            geometry_name: name,
            geometry_data: data,
            ..self
        })
    }

    /// The skin ID.
    #[doc(alias = "Skin::getSkinId")]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The RGBA image.
    #[doc(alias = "Skin::getSkinData")]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The RGBA cape image, empty without a cape.
    #[doc(alias = "Skin::getCapeData")]
    pub fn cape(&self) -> &[u8] {
        &self.cape
    }

    /// The geometry name.
    #[doc(alias = "Skin::getGeometryName")]
    pub fn geometry_name(&self) -> &str {
        &self.geometry_name
    }

    /// The custom geometry JSON, empty for a vanilla geometry.
    #[doc(alias = "Skin::getGeometryData")]
    pub fn geometry_data(&self) -> &[u8] {
        &self.geometry_data
    }

    /// The skin as the protocol carries it (the player list and `PlayerSkin`).
    pub(crate) fn to_protocol(&self) -> protocol::Skin {
        let (_, width, height) = dimensions(self.data.len()).unwrap_or(SIZES[0]);
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
            cape_image_width: if has_cape { 64 } else { 0 },
            cape_image_height: if has_cape { 32 } else { 0 },
            cape_data: self.cape.clone(),
            skin_geometry: self.geometry_data.clone(),
            geometry_data_engine_version: b"0.0.0".to_vec(),
            arm_size: 1,
            trusted: true,
            ..protocol::Skin::default()
        }
    }
}

impl std::fmt::Debug for Skin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
    SIZES.into_iter().find(|&(size, _, _)| size == len)
}

#[cfg(test)]
mod tests {
    use super::*;

    // pmmp: Skin::__construct, Skin::getSkinId, Skin::getSkinData, Skin::getCapeData, Skin::getGeometryName, Skin::getGeometryData, Skin
    #[test]
    fn skins_take_only_accepted_sizes_and_safe_geometry_names() {
        for (size, w, h) in SIZES {
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
        let caped = skin.clone().with_cape(vec![1; CAPE_SIZE]).unwrap();
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
}
