//! Resource packs: the `.mcpack` and `.zip` files players download when they join.
//!
//! The packs come from the `resource-packs` setting (files, a relative path relative to the pack
//! folder `data/packs`, each with an optional `<file>.key` content key next to it) and from
//! [`ServerBuilder::resource_pack`]. They are read once at start, held in memory and offered in
//! that order, the first on top of the stack. A pack file or key file that cannot be read, a pack
//! file that breaks a limit, and the pack file that takes the total size over the limit stop the
//! start with an error naming that file; a pack count over the limit, a total size the packs
//! given in code take over the limit, and a repeated UUID stop it without a file name.
//!
//! Limits: a pack is at most 256 MiB, at most 64 packs are offered, and they total at most
//! 512 MiB (these bound the memory the packs take). A pack's `manifest.json` is at most 256 KiB.
//!
//! The manifest is the root `manifest.json`, else the shortest `.../manifest.json`; a pack with
//! neither but a root `pack_manifest.json` (the old format) is refused as old. The JSON may hold comments and trailing commas.
//! `format_version`, `header.name`, `header.uuid`,
//! `header.version` and `modules` (each with `type`, `uuid` and `version`) are required. A version
//! is three whole numbers, as an array or as `"x.y.z"` text (gophertunnel reads both).
//!
//! Players keep a downloaded pack by its UUID and version: raise `header.version` when the
//! contents of a pack change, or players keep the old pack.
//!
//! Anchor reads finished archives only: it neither zips a folder nor watches files, so a pack
//! that changed on disk is offered after the next start, once it is zipped.

mod manifest;
mod zip;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::ServerBuilder;
use crate::protocol::consts::ResourcePackType;
use crate::protocol::packet::{ResourcePackStack, ResourcePacksInfo};
use crate::protocol::{StackResourcePack, TexturePackInfo, Uuid};
use manifest::Manifest;

/// The size of a content key, in bytes.
const CONTENT_KEY_BYTES: usize = 32;

/// The pack folder's name, in the data folder.
pub(crate) const FOLDER: &str = "packs";

/// The largest key file read: a key and a line break, with room to spare.
const MAX_KEY_FILE_BYTES: u64 = 64;

/// The largest pack file.
pub(crate) const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;

/// The most packs offered.
const MAX_PACKS: usize = 64;

/// The most bytes all offered packs hold together.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// A resource pack: a zip archive with a valid `manifest.json`, held in memory.
#[doc(alias = "ResourcePack")]
#[doc(alias = "ZippedResourcePack")]
pub struct Pack {
    manifest: Manifest,
    content_key: Option<String>,
    size: u64,
    sha256: [u8; 32],
    bytes: Box<[u8]>,
}

impl Pack {
    /// A pack read from the bytes of its archive, for packs a program builds or embeds.
    ///
    /// The bytes may be at most 256 MiB, hold a zip archive Anchor reads (stored or DEFLATE
    /// entries; no zip64, encryption or multiple disks; entry names that stay inside the pack
    /// folder) and a valid `manifest.json` (see the module docs).
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] for bytes over the limit, [`Error::Archive`] for an archive Anchor
    /// does not read, and the manifest errors ([`Error::ManifestMissing`], [`Error::OldFormat`],
    /// [`Error::ManifestTooLarge`], [`Error::Manifest`], [`Error::Uuid`], [`Error::Version`]) for
    /// a manifest that is missing or invalid.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        let size = u64::try_from(bytes.len())
            .ok()
            .filter(|&size| size <= MAX_PACK_BYTES)
            .ok_or(Error::TooLarge {
                limit: MAX_PACK_BYTES,
            })?;
        let manifest = manifest::read(&zip::entries(&bytes)?)?;
        Ok(Self {
            manifest,
            content_key: None,
            size,
            sha256: crate::crypto::sha256(&[&bytes]),
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// The pack read from the file at `path` (`.mcpack` or `.zip`) and, if `<path>.key` exists,
    /// its content key ([`with_content_key`](Self::with_content_key); line breaks at its end are
    /// ignored).
    ///
    /// This does blocking file I/O: call it before the server starts.
    ///
    /// # Errors
    ///
    /// [`Error::File`], naming `path`, with the cause: [`Error::Extension`] for another file
    /// name ending, [`Error::Read`] for a file that cannot be read, [`Error::Key`] for a key
    /// that is not 32 bytes of text, [`Error::KeyFile`] for a key file that is there but cannot
    /// be read, and the errors of [`from_bytes`](Self::from_bytes).
    #[doc(alias = "ZippedResourcePack::__construct")]
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::read(path).map_err(|source| Error::File {
            path: path.to_owned(),
            source: Box::new(source),
        })
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

    /// The pack's name (`header.name`).
    #[doc(alias = "ResourcePack::getPackName")]
    #[doc(alias = "ZippedResourcePack::getPackName")]
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The `ResourcePackDataInfo` pack type (gophertunnel's rules, from the module types).
    pub(crate) fn pack_type(&self) -> u8 {
        let m = &self.manifest;
        let code = m.behaviours || m.scripts;
        u8::from(if m.world_template {
            ResourcePackType::WorldTemplate
        } else if m.textures && code {
            ResourcePackType::Addon
        } else if code {
            ResourcePackType::Behaviour
        } else if m.textures {
            ResourcePackType::Resources
        } else {
            ResourcePackType::Skins
        })
    }

    /// The SHA-256 of the archive, which players check the download against.
    #[doc(alias = "ResourcePack::getSha256")]
    #[doc(alias = "ZippedResourcePack::getSha256")]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The size of the archive in bytes.
    #[doc(alias = "ResourcePack::getPackSize")]
    #[doc(alias = "ZippedResourcePack::getPackSize")]
    pub fn size_bytes(&self) -> u64 {
        self.size
    }

    /// The pack's UUID (`header.uuid` of its manifest).
    #[doc(alias = "ResourcePack::getPackId")]
    #[doc(alias = "ZippedResourcePack::getPackId")]
    pub fn uuid(&self) -> Uuid {
        self.manifest.uuid
    }

    /// The pack's version as `major.minor.patch` (`header.version`). Players keep a downloaded
    /// pack by its UUID and version, so a pack whose contents changed needs a higher one.
    #[doc(alias = "ResourcePack::getPackVersion")]
    #[doc(alias = "ZippedResourcePack::getPackVersion")]
    pub fn version(&self) -> &str {
        &self.manifest.version
    }

    /// The same pack with the key its contents are encrypted with. Anchor does not decrypt the
    /// pack: players get the key to decrypt it themselves.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] unless the key is 32 bytes.
    #[doc(alias = "ResourcePackManager::setPackEncryptionKey")]
    pub fn with_content_key(mut self, key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.len() != CONTENT_KEY_BYTES {
            return Err(Error::Key);
        }
        self.content_key = Some(key);
        Ok(self)
    }

    /// The pack read from the file at `path` with its key file, without naming `path` in an
    /// error.
    fn read(path: &Path) -> Result<Self> {
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
        match crate::fs::read_bounded(Path::new(&key_path), MAX_KEY_FILE_BYTES) {
            Ok(key) => {
                let key = String::from_utf8(key).map_err(|_| Error::Key)?;
                pack.with_content_key(key.trim_end_matches(['\r', '\n']))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(pack),
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Err(Error::Key),
            Err(source) => Err(Error::KeyFile {
                path: key_path.into(),
                source,
            }),
        }
    }
}

impl fmt::Debug for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The content key is a secret: only whether there is one.
        f.debug_struct("Pack")
            .field("uuid", &self.uuid())
            .field("name", &self.name())
            .field("version", &self.version())
            .field("size", &self.size_bytes())
            .field("encrypted", &self.content_key.is_some())
            .finish_non_exhaustive()
    }
}

/// The packs offered to joining players and what the session sends of them, built once at
/// start.
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
    /// The offer of the files at `paths`, then `extra`, with `acceptance` of the packs. Blocking
    /// file I/O: run it before the server starts.
    pub(crate) fn load(
        paths: &[PathBuf],
        extra: Vec<Pack>,
        acceptance: Acceptance,
    ) -> Result<Self> {
        check_count(paths.len().saturating_add(extra.len()))?;
        let mut packs = Vec::with_capacity(paths.len() + extra.len());
        let mut total = 0_u64;
        for path in paths {
            let pack = Pack::load(path)?;
            // The same limit as `new` checks, so a total over it stops before the rest is read.
            total = add_to_total(total, pack.size_bytes(), path)?;
            packs.push(pack);
        }
        packs.extend(extra);
        Self::new(packs, acceptance)
    }

    /// The offer of `packs`, in stack order, with `acceptance` of them.
    pub(crate) fn new(packs: Vec<Pack>, acceptance: Acceptance) -> Result<Self> {
        check_count(packs.len())?;
        check_total(
            packs
                .iter()
                .fold(0_u64, |t, p| t.saturating_add(p.size_bytes())),
        )?;
        let mut index = BTreeMap::new();
        for (i, pack) in packs.iter().enumerate() {
            if index.insert(pack.uuid(), i).is_some() {
                return Err(Error::Duplicate(pack.uuid()));
            }
        }
        let ids = packs
            .iter()
            .map(|p| format!("{}_{}", p.uuid(), p.version()))
            .collect();
        // The values the official server sends for an empty offer; the packs as gophertunnel
        // lists them.
        let info = ResourcePacksInfo {
            texture_pack_required: acceptance == Acceptance::Required,
            world_template_version: "0.0.0".to_owned(),
            texture_packs: packs
                .iter()
                .map(|p| TexturePackInfo {
                    uuid: p.uuid(),
                    version: p.version().to_owned(),
                    size: p.size_bytes(),
                    content_key: p.content_key().unwrap_or_default().to_owned(),
                    content_identity: p
                        .content_key()
                        .map(|_| p.uuid().to_string())
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
                    uuid: p.uuid().to_string(),
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

    /// The index of the pack a client names `uuid` or `uuid_version` (gophertunnel: a version,
    /// when given, must be the pack's).
    pub(crate) fn find(&self, name: &str) -> Option<usize> {
        let (uuid, version) = match name.split_once('_') {
            Some((uuid, version)) => (uuid, Some(version)),
            None => (name, None),
        };
        let index = *self.index.get(&uuid.parse().ok()?)?;
        let pack = self.packs.get(index)?;
        match version {
            Some(v) if !v.is_empty() && v != pack.version() => None,
            _ => Some(index),
        }
    }

    /// The `uuid_version` name of pack `index`, which comes from [`find`](Self::find).
    #[expect(
        clippy::expect_used,
        reason = "invariant: every caller takes `index` from `find`"
    )]
    pub(crate) fn id(&self, index: usize) -> &str {
        self.ids.get(index).expect("the pack index is in range")
    }

    /// `ResourcePacksInfo`, sent after the login.
    pub(crate) fn info(&self) -> &ResourcePacksInfo {
        &self.info
    }

    /// The offered packs, in stack order.
    pub(crate) fn packs(&self) -> &[Pack] {
        &self.packs
    }

    /// `ResourcePackStack`, sent when the client has every pack.
    pub(crate) fn stack(&self) -> &ResourcePackStack {
        &self.stack
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

/// Whether players must accept the offered packs to join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Acceptance {
    /// Players may join without the packs.
    Optional,
    /// Players must accept the packs to join.
    Required,
}

impl From<bool> for Acceptance {
    fn from(required: bool) -> Self {
        if required {
            Self::Required
        } else {
            Self::Optional
        }
    }
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
    /// The pack is larger than `limit` bytes.
    TooLarge {
        /// The largest size accepted.
        limit: u64,
    },
    /// The file is not a zip archive Anchor reads; the text says why.
    Archive(&'static str),
    /// The archive holds no `manifest.json`.
    ManifestMissing,
    /// The archive holds no `manifest.json` but a root `pack_manifest.json`: the old pack format.
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
    /// The content key is not 32 bytes of text, or its key file is longer than a key.
    Key,
    /// The key file at `path` is there but cannot be read.
    KeyFile {
        /// The key file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
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
            Self::KeyFile { path, .. } => write!(f, "cannot read the key file {}", path.display()),
            Self::TooMany { limit } => write!(f, "more than {limit} resource packs"),
            Self::TotalTooLarge { limit } => {
                write!(f, "the resource packs total more than {limit} bytes")
            }
            Self::Duplicate(uuid) => {
                write!(f, "two resource packs have the uuid {uuid}")
            }
            Self::File { path, .. } => write!(f, "resource pack {}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Manifest(e) => Some(e.as_ref()),
            Self::KeyFile { source, .. } => Some(source),
            Self::File { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

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

/// The running total of the pack sizes with the pack at `path` of `size` bytes added, or the
/// error naming that pack when the total passes [`MAX_TOTAL_BYTES`].
fn add_to_total(total: u64, size: u64, path: &Path) -> Result<u64> {
    let total = total.saturating_add(size);
    check_total(total).map_err(|source| Error::File {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    Ok(total)
}

/// Refuses more than [`MAX_PACKS`] packs.
fn check_count(count: usize) -> Result<()> {
    if count > MAX_PACKS {
        return Err(Error::TooMany { limit: MAX_PACKS });
    }
    Ok(())
}

/// Refuses packs totalling more than [`MAX_TOTAL_BYTES`].
fn check_total(total: u64) -> Result<()> {
    if total > MAX_TOTAL_BYTES {
        return Err(Error::TotalTooLarge {
            limit: MAX_TOTAL_BYTES,
        });
    }
    Ok(())
}

/// The bytes of the file at `path`, at most `limit`; a longer file is [`Error::TooLarge`].
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::fs::read_bounded(path, limit).map_err(|e| match e.kind() {
        io::ErrorKind::FileTooLarge => Error::TooLarge { limit },
        _ => Error::Read(e),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::pack::zip::tests::{File, archive};
    use crate::save::tests::TempDir;

    /// The UUID of the test packs.
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

    // api: ResourcePack::getPackId, ZippedResourcePack::getPackId, ResourcePack::getPackName,
    // api: ZippedResourcePack::getPackName, ResourcePack::getPackVersion,
    // api: ZippedResourcePack::getPackVersion, ResourcePack::getPackSize,
    // api: ZippedResourcePack::getPackSize, ResourcePack::getSha256,
    // api: ZippedResourcePack::getSha256, ResourcePack::getPackChunk,
    // api: ZippedResourcePack::getPackChunk, ResourcePack, ZippedResourcePack
    #[test]
    fn a_pack_reads_its_manifest_and_hashes_its_bytes() {
        let bytes = pack_bytes(UUID, "1.2.3", 100);
        let pack = Pack::from_bytes(bytes.clone()).unwrap();
        assert_eq!(pack.uuid().to_string(), UUID);
        assert_eq!(pack.name(), "Test");
        assert_eq!(pack.version(), "1.2.3");
        assert_eq!(pack.size_bytes(), u64::try_from(bytes.len()).unwrap());
        assert_eq!(pack.sha256(), &crate::crypto::sha256(&[&bytes]));
        assert_eq!(pack.bytes(), bytes.as_slice());
        assert_eq!(pack.pack_type(), u8::from(ResourcePackType::Resources));
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

    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    // api: ZippedResourcePack::__construct, ResourcePackManager::setPackEncryptionKey,
    // api: ResourcePackManager::getPackEncryptionKey, ResourcePackException
    #[test]
    fn a_pack_file_loads_with_its_key_file() {
        let dir = TempDir::new("pack-key");
        let path = dir.0.join("a.mcpack");
        write(&path, &pack_bytes(UUID, "1.0.0", 10));
        let pack = Pack::load(&path).unwrap();
        assert_eq!(pack.content_key(), None);
        let key = "0123456789abcdef0123456789abcdef";
        write(&dir.0.join("a.mcpack.key"), format!("{key}\r\n").as_bytes());
        let pack = Pack::load(&path).unwrap();
        assert_eq!(pack.content_key(), Some(key));
        assert!(!format!("{pack:?}").contains(key), "the key is secret");
        let offer = Offer::new(vec![pack], Acceptance::Optional).unwrap();
        assert!(!format!("{offer:?}").contains(key), "the key is secret");
        let pack = Pack::load(&path).unwrap();

        write(&dir.0.join("a.mcpack.key"), b"short");
        let error = Pack::load(&path).unwrap_err();
        assert!(matches!(&error, Error::File { source, .. } if matches!(**source, Error::Key)));
        assert!(error.to_string().contains("a.mcpack"));
        write(&dir.0.join("a.mcpack.key"), &[b'k'; 100]);
        assert!(Pack::load(&path).is_err());

        assert!(matches!(
            pack.with_content_key("x".repeat(31)),
            Err(Error::Key)
        ));

        let missing = Pack::load(dir.0.join("missing.zip")).unwrap_err();
        assert!(
            matches!(&missing, Error::File { source, .. } if matches!(**source, Error::Read(_)))
        );
        let txt = dir.0.join("a.txt");
        write(&txt, &pack_bytes(UUID, "1.0.0", 10));
        let wrong = Pack::load(&txt).unwrap_err();
        assert!(
            matches!(&wrong, Error::File { source, .. } if matches!(**source, Error::Extension))
        );
        assert!(std::error::Error::source(&wrong).is_some());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test files")]
    fn a_key_file_that_cannot_be_read_is_named() {
        let dir = TempDir::new("pack-unreadable-key");
        let path = dir.0.join("a.mcpack");
        write(&path, &pack_bytes(UUID, "1.0.0", 10));
        // A folder in the key file's place is there but cannot be read as a file.
        std::fs::create_dir(dir.0.join("a.mcpack.key")).unwrap();
        let error = Pack::load(&path).unwrap_err();
        let mut text = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            text = format!("{text}: {cause}");
            source = cause.source();
        }
        assert!(text.contains("a.mcpack.key"), "{text}");
    }

    /// A pack with UUID `uuid`, version `version` and `extra` bytes of texture.
    pub(crate) fn pack(uuid: &str, version: &str, extra: usize) -> Pack {
        Pack::from_bytes(pack_bytes(uuid, version, extra)).unwrap()
    }

    #[test]
    fn an_offer_lists_the_packs_in_order_and_finds_them_by_name() {
        let other = "11111111-2222-3333-4444-555555555555";
        let keyed = pack(other, "2.0.0", 10)
            .with_content_key("k".repeat(32))
            .unwrap();
        let offer = Offer::new(vec![pack(UUID, "1.0.0", 10), keyed], Acceptance::Required).unwrap();
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

    // api: ResourcePackManager::setResourceStack
    #[test]
    fn packs_given_in_code_follow_the_configured_files() {
        let other = "11111111-2222-3333-4444-555555555555";
        let builder = crate::Server::builder().resource_pack(pack(UUID, "1.0.0", 1));
        let dir = TempDir::new("pack-order");
        let file = dir.0.join("b.zip");
        write(&file, &pack_bytes(other, "2.0.0", 1));
        let offer = Offer::load(
            std::slice::from_ref(&file),
            builder.packs,
            Acceptance::Optional,
        )
        .unwrap();
        let order: Vec<String> = offer.packs().iter().map(|p| p.uuid().to_string()).collect();
        assert_eq!(order, [other, UUID]);

        let missing = dir.0.join("missing.mcpack");
        let error = Offer::load(&[file, missing], Vec::new(), Acceptance::Optional).unwrap_err();
        assert!(error.to_string().contains("missing.mcpack"), "{error}");
    }

    #[test]
    fn offers_refuse_duplicates_and_too_many_packs() {
        let error = Offer::new(
            vec![pack(UUID, "1.0.0", 1), pack(UUID, "2.0.0", 1)],
            Acceptance::Optional,
        );
        assert!(matches!(error, Err(Error::Duplicate(_))));
        let paths = vec![PathBuf::from("a.zip"); MAX_PACKS + 1];
        assert!(matches!(
            Offer::load(&paths, Vec::new(), Acceptance::Optional),
            Err(Error::TooMany { .. })
        ));
        let empty = Offer::new(Vec::new(), Acceptance::Optional).unwrap();
        assert!(empty.info().texture_packs.is_empty());
        assert_eq!(empty.stack().base_game_version, "*");
    }

    #[test]
    fn a_total_over_the_limit_names_the_pack_that_passes_it() {
        let path = Path::new("big.zip");
        assert_eq!(
            add_to_total(MAX_TOTAL_BYTES - 1, 1, path).unwrap(),
            MAX_TOTAL_BYTES
        );
        let error = add_to_total(MAX_TOTAL_BYTES, 1, path).unwrap_err();
        assert!(matches!(
            &error,
            Error::File { path, source }
                if path == Path::new("big.zip")
                    && matches!(**source, Error::TotalTooLarge { .. })
        ));
        assert!(matches!(
            check_total(MAX_TOTAL_BYTES + 1),
            Err(Error::TotalTooLarge { .. })
        ));
    }

    #[test]
    fn pack_types_follow_gophertunnel() {
        let mut p = pack(UUID, "1.0.0", 1);
        assert_eq!(p.pack_type(), u8::from(ResourcePackType::Resources));
        p.manifest.behaviours = true;
        assert_eq!(p.pack_type(), u8::from(ResourcePackType::Addon));
        p.manifest.textures = false;
        assert_eq!(p.pack_type(), u8::from(ResourcePackType::Behaviour));
        p.manifest.behaviours = false;
        assert_eq!(p.pack_type(), u8::from(ResourcePackType::Skins));
        p.manifest.world_template = true;
        assert_eq!(p.pack_type(), u8::from(ResourcePackType::WorldTemplate));
    }
}
