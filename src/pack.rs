//! Resource packs: the `.mcpack` and `.zip` files players download when they join.
//!
//! The packs come from the `resource-packs` setting (files relative to the pack folder
//! `data/packs`, each with an optional `<file>.key` content key next to it) and from
//! [`ServerBuilder::resource_pack`]. They are read once at start, held in memory and offered in
//! that order, the first on top of the stack: a pack that cannot be read, or breaks a limit,
//! stops the start with an error naming its file.
//!
//! Earlier versions kept the packs in `packs` next to the settings file. At start that folder is
//! moved to `data/packs` with one rename, unless `data/packs` exists: then it is left as it is
//! with a warning. Only `data/packs` is read.
//!
//! Limits: a pack is at most 256 MiB, at most 64 packs are offered, and they total at most
//! 512 MiB (PMMP has none; these bound the memory the packs take). A pack's `manifest.json` is at
//! most 256 KiB.

mod manifest;
mod zip;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::ServerBuilder;
use crate::protocol::Uuid;
use crate::protocol::packet::{ResourcePackStack, ResourcePacksInfo};
use crate::protocol::{StackResourcePack, TexturePackInfo};

use self::manifest::Manifest;

/// The pack folder's name, in the data folder (and, in earlier versions, next to the settings
/// file).
pub(crate) const FOLDER: &str = "packs";

/// The largest pack file.
pub(crate) const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
/// The most packs offered.
const MAX_PACKS: usize = 64;
/// The most bytes all offered packs hold together.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// A content key is 32 bytes (PMMP `ResourcePackManager`).
const KEY_LEN: usize = 32;
/// The largest key file read: a key and a line break, with room to spare.
const MAX_KEY_FILE_BYTES: u64 = 64;
/// `ResourcePackDataInfo` pack types (gophertunnel `ResourcePackType*`).
const TYPE_ADDON: u8 = 1;
const TYPE_BEHAVIOUR: u8 = 4;
const TYPE_RESOURCES: u8 = 6;
const TYPE_SKINS: u8 = 7;
const TYPE_WORLD_TEMPLATE: u8 = 8;

/// A resource pack: a zip archive with a valid `manifest.json`, held in memory.
#[doc(alias = "ResourcePack")]
#[doc(alias = "ZippedResourcePack")]
pub struct Pack {
    manifest: Manifest,
    content_key: Option<String>,
    sha256: [u8; 32],
    bytes: Box<[u8]>,
}

impl Pack {
    /// Reads a pack from the bytes of its archive, for packs a program builds or embeds.
    ///
    /// Fails if the bytes are larger than 256 MiB, are not a zip archive Anchor reads (stored or
    /// DEFLATE entries; no zip64, encryption or multiple disks; entry names that stay inside the
    /// pack folder), or hold no valid `manifest.json` (PMMP's rules, see the module docs).
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_PACK_BYTES) {
            return Err(Error::TooLarge {
                limit: MAX_PACK_BYTES,
            });
        }
        let manifest = manifest::read(&zip::entries(&bytes).map_err(Error::Archive)?)?;
        Ok(Self {
            manifest,
            content_key: None,
            sha256: crate::crypto::sha256(&[&bytes]),
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// Reads the pack file at `path` (`.mcpack` or `.zip`) and, if `<path>.key` exists, its
    /// content key ([`with_content_key`](Self::with_content_key); line breaks at its end are
    /// ignored, as PMMP does).
    ///
    /// This does blocking file I/O: call it before the server starts. Fails like
    /// [`from_bytes`](Self::from_bytes), or if a file cannot be read; the error names `path`.
    #[doc(alias = "ZippedResourcePack::__construct")]
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let load = || {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !["mcpack", "zip"]
                .iter()
                .any(|e| extension.eq_ignore_ascii_case(e))
            {
                return Err(Error::Extension);
            }
            let pack = Self::from_bytes(read_file(path, MAX_PACK_BYTES)?)?;
            let mut key_path = path.as_os_str().to_owned();
            key_path.push(".key");
            match read_file(Path::new(&key_path), MAX_KEY_FILE_BYTES) {
                Ok(key) => {
                    let key = String::from_utf8(key).map_err(|_| Error::Key)?;
                    pack.with_content_key(key.trim_end_matches(['\r', '\n']))
                }
                Err(Error::Read(e)) if e.kind() == io::ErrorKind::NotFound => Ok(pack),
                Err(Error::TooLarge { .. }) => Err(Error::Key),
                Err(e) => Err(e),
            }
        };
        load().map_err(|source| Error::File {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    /// The pack with the key its contents are encrypted with. Anchor does not decrypt the pack:
    /// players get the key to decrypt it themselves. Fails unless the key is 32 bytes.
    #[doc(alias = "ResourcePackManager::setPackEncryptionKey")]
    pub fn with_content_key(mut self, key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.len() != KEY_LEN {
            return Err(Error::Key);
        }
        self.content_key = Some(key);
        Ok(self)
    }

    /// The pack's UUID (`header.uuid` of its manifest).
    #[doc(alias = "ResourcePack::getPackId")]
    #[doc(alias = "ZippedResourcePack::getPackId")]
    pub fn uuid(&self) -> Uuid {
        self.manifest.uuid
    }

    /// The pack's name (`header.name`).
    #[doc(alias = "ResourcePack::getPackName")]
    #[doc(alias = "ZippedResourcePack::getPackName")]
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The pack's version as `major.minor.patch` (`header.version`).
    #[doc(alias = "ResourcePack::getPackVersion")]
    #[doc(alias = "ZippedResourcePack::getPackVersion")]
    pub fn version(&self) -> &str {
        &self.manifest.version
    }

    /// The size of the archive in bytes.
    #[doc(alias = "ResourcePack::getPackSize")]
    #[doc(alias = "ZippedResourcePack::getPackSize")]
    pub fn size(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    /// The SHA-256 of the archive, which players check the download against.
    #[doc(alias = "ResourcePack::getSha256")]
    #[doc(alias = "ZippedResourcePack::getSha256")]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The archive's bytes, as players download them.
    #[doc(alias = "ResourcePack::getPackChunk")]
    #[doc(alias = "ZippedResourcePack::getPackChunk")]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The content key players get with the pack, if it is encrypted.
    #[doc(alias = "ResourcePackManager::getPackEncryptionKey")]
    pub fn content_key(&self) -> Option<&str> {
        self.content_key.as_deref()
    }

    /// The `ResourcePackDataInfo` pack type (gophertunnel's rules, from the module types).
    pub(crate) fn pack_type(&self) -> u8 {
        let m = &self.manifest;
        let code = m.behaviours || m.scripts;
        if m.world_template {
            TYPE_WORLD_TEMPLATE
        } else if m.textures && code {
            TYPE_ADDON
        } else if code {
            TYPE_BEHAVIOUR
        } else if m.textures {
            TYPE_RESOURCES
        } else {
            TYPE_SKINS
        }
    }
}

impl fmt::Debug for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The content key is a secret: only whether there is one.
        f.debug_struct("Pack")
            .field("uuid", &uuid_text(self.uuid()))
            .field("name", &self.name())
            .field("version", &self.version())
            .field("size", &self.size())
            .field("encrypted", &self.content_key.is_some())
            .finish_non_exhaustive()
    }
}

impl ServerBuilder {
    /// Offers `pack` to every joining player, after the packs of the `resource-packs` setting
    /// and those added before. `force-resources` decides whether players must accept it.
    #[doc(alias = "ResourcePackManager::setResourceStack")]
    #[must_use]
    pub fn resource_pack(mut self, pack: Pack) -> Self {
        self.packs.push(pack);
        self
    }
}

/// Reads at most `limit` bytes of the file at `path`; a longer file is [`Error::TooLarge`].
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::fs::read_bounded(path, limit).map_err(|e| match e.kind() {
        io::ErrorKind::FileTooLarge => Error::TooLarge { limit },
        _ => Error::Read(e),
    })
}

/// Moves the pack folder of earlier versions, `old`, to the pack folder `new` with one rename,
/// so a failure leaves it where it was. Nothing happens unless `old` is a folder; if `new` exists
/// too, `old` is left as it is and a warning says so. The folder holding `new` must exist (the
/// server file made the data folder). Blocking file I/O: run it before the server starts.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: runs on the blocking pool before the server starts"
)]
pub(crate) fn move_old_folder(old: &Path, new: &Path) -> Result<()> {
    let failed = |source| Error::Folder {
        from: old.to_owned(),
        to: new.to_owned(),
        source,
    };
    match std::fs::metadata(old) {
        Ok(found) if found.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(failed(e)),
    }
    match std::fs::symlink_metadata(new) {
        Ok(_) => {
            warn!(
                old = %old.display(),
                new = %new.display(),
                "both pack folders exist: the old one is left as it is and not read"
            );
            return Ok(());
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(e)),
    }
    std::fs::rename(old, new).map_err(failed)?;
    info!(from = %old.display(), to = %new.display(), "pack folder moved");
    Ok(())
}

/// The packs offered to joining players and what the session sends of them, built once at
/// start.
#[derive(Default)]
pub(crate) struct Offer {
    packs: Vec<Pack>,
    /// `uuid_version` of each pack: how the client names it in requests and how the server
    /// names it in replies (gophertunnel).
    ids: Vec<String>,
    /// Pack index by UUID.
    index: BTreeMap<Uuid, usize>,
    info: ResourcePacksInfo,
    stack: ResourcePackStack,
}

impl Offer {
    /// Loads the files at `paths`, adds `extra` after them and checks the limits. Players must
    /// accept the packs when `force` is set. Blocking file I/O: run it before the server starts.
    pub(crate) fn load(paths: &[PathBuf], extra: Vec<Pack>, force: bool) -> Result<Self> {
        if paths.len().saturating_add(extra.len()) > MAX_PACKS {
            return Err(Error::TooMany { limit: MAX_PACKS });
        }
        let mut packs = Vec::with_capacity(paths.len() + extra.len());
        let mut total = 0u64;
        for path in paths {
            let pack = Pack::load(path)?;
            total = total.saturating_add(pack.size());
            if total > MAX_TOTAL_BYTES {
                return Err(Error::TotalTooLarge {
                    limit: MAX_TOTAL_BYTES,
                });
            }
            packs.push(pack);
        }
        packs.extend(extra);
        Self::new(packs, force)
    }

    /// The offer of `packs`, in stack order.
    pub(crate) fn new(packs: Vec<Pack>, force: bool) -> Result<Self> {
        if packs.len() > MAX_PACKS {
            return Err(Error::TooMany { limit: MAX_PACKS });
        }
        let total = packs.iter().fold(0u64, |t, p| t.saturating_add(p.size()));
        if total > MAX_TOTAL_BYTES {
            return Err(Error::TotalTooLarge {
                limit: MAX_TOTAL_BYTES,
            });
        }
        let mut index = BTreeMap::new();
        for (i, pack) in packs.iter().enumerate() {
            if index.insert(pack.uuid(), i).is_some() {
                return Err(Error::Duplicate(pack.uuid()));
            }
        }
        let ids = packs
            .iter()
            .map(|p| format!("{}_{}", uuid_text(p.uuid()), p.version()))
            .collect();
        // BDS's values for an empty offer (P6); the packs as gophertunnel lists them.
        let info = ResourcePacksInfo {
            texture_pack_required: force,
            world_template_version: "0.0.0".to_owned(),
            texture_packs: packs
                .iter()
                .map(|p| TexturePackInfo {
                    uuid: p.uuid(),
                    version: p.version().to_owned(),
                    size: p.size(),
                    content_key: p.content_key().unwrap_or_default().to_owned(),
                    content_identity: p
                        .content_key()
                        .map(|_| uuid_text(p.uuid()))
                        .unwrap_or_default(),
                    ..TexturePackInfo::default()
                })
                .collect(),
            ..ResourcePacksInfo::default()
        };
        let stack = ResourcePackStack {
            base_game_version: "*".to_owned(),
            texture_packs: packs
                .iter()
                .map(|p| StackResourcePack {
                    uuid: uuid_text(p.uuid()),
                    version: p.version().to_owned(),
                    sub_pack_name: String::new(),
                })
                .collect(),
            ..ResourcePackStack::default()
        };
        Ok(Self {
            packs,
            ids,
            index,
            info,
            stack,
        })
    }

    /// The offered packs, in stack order.
    pub(crate) fn packs(&self) -> &[Pack] {
        &self.packs
    }

    /// `ResourcePacksInfo`, sent after the login.
    pub(crate) fn info(&self) -> &ResourcePacksInfo {
        &self.info
    }

    /// `ResourcePackStack`, sent when the client has every pack.
    pub(crate) fn stack(&self) -> &ResourcePackStack {
        &self.stack
    }

    /// The `uuid_version` name of pack `index`.
    pub(crate) fn id(&self, index: usize) -> &str {
        self.ids.get(index).map_or("", String::as_str)
    }

    /// The index of the pack a client names `uuid` or `uuid_version` (gophertunnel: a version,
    /// when given, must be the pack's).
    pub(crate) fn find(&self, name: &str) -> Option<usize> {
        let (uuid, version) = match name.split_once('_') {
            Some((uuid, version)) => (uuid, Some(version)),
            None => (name, None),
        };
        let index = *self.index.get(&crate::login::parse_uuid(uuid)?)?;
        let pack = self.packs.get(index)?;
        match version {
            Some(v) if !v.is_empty() && v != pack.version() => None,
            _ => Some(index),
        }
    }
}

impl fmt::Debug for Offer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The packets hold the content keys; the packs show only whether they have one.
        f.debug_struct("Offer")
            .field("packs", &self.packs)
            .field("required", &self.info.texture_pack_required)
            .finish_non_exhaustive()
    }
}

/// The lowercase hyphenated text of `uuid`, as packs and the client write it.
fn uuid_text(uuid: Uuid) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in uuid.as_bytes().iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        for nibble in [byte >> 4, byte & 0xf] {
            out.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    out
}

/// A pack that cannot be offered.
#[doc(alias = "ResourcePackException")]
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A file cannot be read.
    Read(io::Error),
    /// The file name does not end in `.mcpack` or `.zip`.
    Extension,
    /// The pack, or its key file, is larger than `limit` bytes.
    TooLarge {
        /// The largest size accepted.
        limit: u64,
    },
    /// The file is not a zip archive Anchor reads; the text says why.
    Archive(&'static str),
    /// The archive holds no `manifest.json`.
    ManifestMissing,
    /// The archive holds a `pack_manifest.json`: the old pack format.
    OldFormat,
    /// `manifest.json` is larger than `limit` bytes.
    ManifestTooLarge {
        /// The largest size accepted.
        limit: usize,
    },
    /// `manifest.json` is not valid JSON of the right shape; the source says why.
    Manifest(Box<dyn std::error::Error + Send + Sync>),
    /// `header.uuid` is not a UUID.
    Uuid,
    /// `header.version` is not three whole numbers.
    Version,
    /// The content key is not 32 bytes of text.
    Key,
    /// More than `limit` packs.
    TooMany {
        /// The most packs.
        limit: usize,
    },
    /// The packs total more than `limit` bytes.
    TotalTooLarge {
        /// The most bytes.
        limit: u64,
    },
    /// Two packs have this UUID.
    Duplicate(Uuid),
    /// The pack file at `path` cannot be offered; the source says why.
    File {
        /// The pack file.
        path: PathBuf,
        /// Why.
        source: Box<Error>,
    },
    /// The pack folder of earlier versions, `from`, cannot be moved to `to`; it is left where it
    /// was.
    Folder {
        /// The old pack folder.
        from: PathBuf,
        /// The pack folder in the data folder.
        to: PathBuf,
        /// Why.
        source: io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => f.write_str("cannot read the file"),
            Self::Extension => f.write_str("the file name does not end in .mcpack or .zip"),
            Self::TooLarge { limit } => write!(f, "larger than {limit} bytes"),
            Self::Archive(what) => write!(f, "invalid zip archive: {what}"),
            Self::ManifestMissing => f.write_str("the archive has no manifest.json"),
            Self::OldFormat => f.write_str("the pack uses the old pack_manifest.json format"),
            Self::ManifestTooLarge { limit } => {
                write!(f, "manifest.json is larger than {limit} bytes")
            }
            Self::Manifest(_) => f.write_str("manifest.json is malformed"),
            Self::Uuid => f.write_str("invalid header uuid in manifest.json"),
            Self::Version => f.write_str("invalid header version in manifest.json"),
            Self::Key => f.write_str("the content key is not 32 bytes of text"),
            Self::TooMany { limit } => write!(f, "more than {limit} resource packs"),
            Self::TotalTooLarge { limit } => {
                write!(f, "the resource packs total more than {limit} bytes")
            }
            Self::Duplicate(uuid) => {
                write!(f, "two resource packs have the uuid {}", uuid_text(*uuid))
            }
            Self::File { path, .. } => write!(f, "resource pack {}", path.display()),
            Self::Folder { from, to, .. } => write!(
                f,
                "cannot move the pack folder {} to {}",
                from.display(),
                to.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) | Self::Folder { source: e, .. } => Some(e),
            Self::Manifest(e) => Some(e.as_ref()),
            Self::File { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// The result of reading a pack.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pack::zip::tests::{File, archive};

    pub(crate) const UUID: &str = "0ad9e0c3-2c63-4a43-9f5c-4c1d6c2e7a10";

    /// A pack archive with UUID `uuid`, version `version` and `extra` bytes of texture.
    pub(crate) fn pack_bytes(uuid: &str, version: &str, extra: usize) -> Vec<u8> {
        let manifest = format!(
            r#"{{"format_version": 2, "header": {{"name": "Test", "uuid": "{uuid}",
            "version": "{version}"}}, "modules": [{{"type": "resources", "uuid": "{uuid}",
            "version": [1, 0, 0]}}]}}"#
        );
        // Bytes that do not compress, so the archive is about `extra` bytes larger.
        let mut rng = crate::protocol::Rng(7);
        let texture: Vec<u8> = (0..extra).map(|_| rng.next().to_le_bytes()[0]).collect();
        archive(&[
            File {
                name: "manifest.json",
                data: manifest.as_bytes(),
                deflate: true,
            },
            File {
                name: "textures/a.png",
                data: &texture,
                deflate: false,
            },
        ])
    }

    pub(crate) fn pack(uuid: &str, version: &str, extra: usize) -> Pack {
        Pack::from_bytes(pack_bytes(uuid, version, extra)).unwrap()
    }

    /// A per-test folder under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("anchor-pack-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).unwrap_or(());
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap_or(());
        }
    }

    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    // pmmp: ResourcePack::getPackId, ZippedResourcePack::getPackId, ResourcePack::getPackName, ZippedResourcePack::getPackName, ResourcePack::getPackVersion, ZippedResourcePack::getPackVersion, ResourcePack::getPackSize, ZippedResourcePack::getPackSize, ResourcePack::getSha256, ZippedResourcePack::getSha256, ResourcePack::getPackChunk, ZippedResourcePack::getPackChunk, ResourcePack, ZippedResourcePack
    #[test]
    fn a_pack_reads_its_manifest_and_hashes_its_bytes() {
        let bytes = pack_bytes(UUID, "1.2.3", 100);
        let pack = Pack::from_bytes(bytes.clone()).unwrap();
        assert_eq!(uuid_text(pack.uuid()), UUID);
        assert_eq!(pack.name(), "Test");
        assert_eq!(pack.version(), "1.2.3");
        assert_eq!(pack.size(), u64::try_from(bytes.len()).unwrap());
        assert_eq!(pack.sha256(), &crate::crypto::sha256(&[&bytes]));
        assert_eq!(pack.bytes(), bytes.as_slice());
        assert_eq!(pack.pack_type(), TYPE_RESOURCES);
        assert_eq!(pack.content_key(), None);
        let debug = format!("{pack:?}");
        assert!(debug.contains(UUID) && debug.contains("encrypted: false"));
    }

    #[test]
    fn oversized_and_broken_bytes_are_refused() {
        let big = vec![0; usize::try_from(MAX_PACK_BYTES).unwrap() + 1];
        assert!(matches!(Pack::from_bytes(big), Err(Error::TooLarge { .. })));
        assert!(matches!(
            Pack::from_bytes(b"PK".to_vec()),
            Err(Error::Archive(_))
        ));
    }

    // pmmp: ZippedResourcePack::__construct, ResourcePackManager::setPackEncryptionKey, ResourcePackManager::getPackEncryptionKey, ResourcePackException
    #[test]
    fn a_pack_file_loads_with_its_key_file() {
        let dir = TempDir::new("key");
        let path = dir.join("a.mcpack");
        write(&path, &pack_bytes(UUID, "1.0.0", 10));
        let pack = Pack::load(&path).unwrap();
        assert_eq!(pack.content_key(), None);
        let key = "0123456789abcdef0123456789abcdef";
        write(&dir.join("a.mcpack.key"), format!("{key}\r\n").as_bytes());
        let pack = Pack::load(&path).unwrap();
        assert_eq!(pack.content_key(), Some(key));
        assert!(!format!("{pack:?}").contains(key), "the key is secret");
        let offer = Offer::new(vec![pack], false).unwrap();
        assert!(!format!("{offer:?}").contains(key), "the key is secret");
        let pack = Pack::load(&path).unwrap();

        write(&dir.join("a.mcpack.key"), b"short");
        let error = Pack::load(&path).unwrap_err();
        assert!(matches!(&error, Error::File { source, .. } if matches!(**source, Error::Key)));
        assert!(error.to_string().contains("a.mcpack"));
        write(&dir.join("a.mcpack.key"), &[b'k'; 100]);
        assert!(Pack::load(&path).is_err());

        assert!(matches!(
            pack.with_content_key("x".repeat(31)),
            Err(Error::Key)
        ));

        let missing = Pack::load(dir.join("missing.zip")).unwrap_err();
        assert!(
            matches!(&missing, Error::File { source, .. } if matches!(**source, Error::Read(_)))
        );
        let txt = dir.join("a.txt");
        write(&txt, &pack_bytes(UUID, "1.0.0", 10));
        let wrong = Pack::load(&txt).unwrap_err();
        assert!(
            matches!(&wrong, Error::File { source, .. } if matches!(**source, Error::Extension))
        );
        assert!(std::error::Error::source(&wrong).is_some());
    }

    #[test]
    fn an_offer_lists_the_packs_in_order_and_finds_them_by_name() {
        let other = "11111111-2222-3333-4444-555555555555";
        let keyed = pack(other, "2.0.0", 10)
            .with_content_key("k".repeat(32))
            .unwrap();
        let offer = Offer::new(vec![pack(UUID, "1.0.0", 10), keyed], true).unwrap();
        let info = offer.info();
        assert!(info.texture_pack_required);
        assert_eq!(info.world_template_version, "0.0.0");
        assert_eq!(info.texture_packs.len(), 2);
        assert_eq!(info.texture_packs[0].content_key, "");
        assert_eq!(info.texture_packs[0].content_identity, "");
        assert_eq!(info.texture_packs[1].content_key, "k".repeat(32));
        assert_eq!(info.texture_packs[1].content_identity, other);
        let stack = offer.stack();
        assert!(!stack.texture_pack_required);
        assert_eq!(stack.base_game_version, "*");
        assert_eq!(stack.texture_packs[0].uuid, UUID);
        assert_eq!(stack.texture_packs[1].version, "2.0.0");
        assert_eq!(offer.id(0), format!("{UUID}_1.0.0"));
        assert_eq!(offer.find(&format!("{UUID}_1.0.0")), Some(0));
        assert_eq!(offer.find(UUID), Some(0));
        assert_eq!(offer.find(&format!("{}_", other.to_uppercase())), Some(1));
        assert_eq!(offer.find(&format!("{UUID}_9.9.9")), None);
        assert_eq!(offer.find("nonsense"), None);
    }

    // pmmp: ResourcePackManager::setResourceStack
    #[test]
    fn packs_given_in_code_follow_the_configured_files() {
        let other = "11111111-2222-3333-4444-555555555555";
        let builder = crate::Server::builder().resource_pack(pack(UUID, "1.0.0", 1));
        let dir = TempDir::new("order");
        let file = dir.join("b.zip");
        write(&file, &pack_bytes(other, "2.0.0", 1));
        let offer = Offer::load(std::slice::from_ref(&file), builder.packs, false).unwrap();
        let order: Vec<String> = offer.packs().iter().map(|p| uuid_text(p.uuid())).collect();
        assert_eq!(order, [other, UUID]);

        let missing = dir.join("missing.mcpack");
        let error = Offer::load(&[file, missing], Vec::new(), false).unwrap_err();
        assert!(error.to_string().contains("missing.mcpack"), "{error}");
    }

    #[test]
    fn offers_refuse_duplicates_and_too_many_packs() {
        let error = Offer::new(vec![pack(UUID, "1.0.0", 1), pack(UUID, "2.0.0", 1)], false);
        assert!(matches!(error, Err(Error::Duplicate(_))));
        let paths = vec![PathBuf::from("a.zip"); MAX_PACKS + 1];
        assert!(matches!(
            Offer::load(&paths, Vec::new(), false),
            Err(Error::TooMany { .. })
        ));
        let empty = Offer::new(Vec::new(), false).unwrap();
        assert!(empty.info().texture_packs.is_empty());
        assert_eq!(empty.stack().base_game_version, "*");
    }

    /// `<dir>/packs` with `a.zip` in it, the old pack folder of `dir`.
    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn old_folder(dir: &TempDir) -> PathBuf {
        let old = dir.join(FOLDER);
        std::fs::create_dir(&old).unwrap();
        write(&old.join("a.zip"), b"old");
        old
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn the_old_pack_folder_moves_into_the_data_folder() {
        let dir = TempDir::new("move");
        let old = old_folder(&dir);
        let data = dir.join("data");
        std::fs::create_dir(&data).unwrap();
        let new = data.join(FOLDER);
        move_old_folder(&old, &new).unwrap();
        assert!(!old.exists());
        assert_eq!(std::fs::read(new.join("a.zip")).unwrap(), b"old");
        // Nothing is left to move: a second start changes nothing.
        move_old_folder(&old, &new).unwrap();
        assert_eq!(std::fs::read(new.join("a.zip")).unwrap(), b"old");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn a_failed_move_leaves_the_old_pack_folder_as_it_was() {
        let dir = TempDir::new("move-fails");
        let old = old_folder(&dir);
        // No data folder: the rename cannot happen.
        let new = dir.join("data").join(FOLDER);
        let error = move_old_folder(&old, &new).unwrap_err();
        assert!(
            matches!(&error, Error::Folder { from, to, .. } if *from == old && *to == new),
            "{error:?}"
        );
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(std::fs::read(old.join("a.zip")).unwrap(), b"old");
        assert!(!new.exists());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn with_both_pack_folders_the_old_one_is_left_alone() {
        let dir = TempDir::new("move-both");
        let old = old_folder(&dir);
        let new = dir.join("data").join(FOLDER);
        std::fs::create_dir_all(&new).unwrap();
        write(&new.join("b.zip"), b"new");
        move_old_folder(&old, &new).unwrap();
        assert_eq!(std::fs::read(old.join("a.zip")).unwrap(), b"old");
        assert!(!new.join("a.zip").exists());
        assert_eq!(std::fs::read(new.join("b.zip")).unwrap(), b"new");
        // A file named `packs` is no pack folder and stays.
        let file = TempDir::new("move-file");
        write(&file.join(FOLDER), b"file");
        move_old_folder(&file.join(FOLDER), &file.join("new")).unwrap();
        assert!(file.join(FOLDER).is_file() && !file.join("new").exists());
    }

    #[test]
    fn pack_types_follow_gophertunnel() {
        let mut p = pack(UUID, "1.0.0", 1);
        assert_eq!(p.pack_type(), TYPE_RESOURCES);
        p.manifest.behaviours = true;
        assert_eq!(p.pack_type(), TYPE_ADDON);
        p.manifest.textures = false;
        assert_eq!(p.pack_type(), TYPE_BEHAVIOUR);
        p.manifest.behaviours = false;
        assert_eq!(p.pack_type(), TYPE_SKINS);
        p.manifest.world_template = true;
        assert_eq!(p.pack_type(), TYPE_WORLD_TEMPLATE);
    }

    #[test]
    fn uuid_text_is_lowercase_and_hyphenated() {
        let uuid = crate::login::parse_uuid("0AD9E0C3-2C63-4A43-9F5C-4C1D6C2E7A10").unwrap();
        assert_eq!(uuid_text(uuid), UUID);
    }
}
