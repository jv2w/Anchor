//! `manifest.json`: where a pack keeps it, and what Anchor reads of it (PMMP
//! `ZippedResourcePack` and its `json\Manifest` classes).
//!
//! The rules follow PMMP: the root `manifest.json`, else the shortest `.../manifest.json`; an
//! old `pack_manifest.json` pack is refused; the JSON may hold comments and trailing commas (PMMP
//! strips them with `Ahc\Json\Comment`); `format_version`, `header.name`, `header.uuid`,
//! `header.version` and `modules` (each with `type`, `uuid` and `version`) are required. A version
//! is three whole numbers, as an array or as `"x.y.z"` text (gophertunnel reads both; PMMP reads
//! only the array).

use serde::Deserialize;

use super::zip::{self, Entry};
use super::{Error, Result};
use crate::protocol::Uuid;

/// The largest `manifest.json` read; real ones are a few KiB.
pub(super) const MAX_MANIFEST_BYTES: usize = 256 * 1024;

const MANIFEST: &[u8] = b"manifest.json";
const NESTED_MANIFEST: &[u8] = b"/manifest.json";
const OLD_MANIFEST: &[u8] = b"pack_manifest.json";
const LEVEL_DAT: &[u8] = b"level.dat";

/// What Anchor keeps of a pack's manifest and contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Manifest {
    pub(super) name: String,
    pub(super) uuid: Uuid,
    /// `x.y.z`.
    pub(super) version: String,
    /// Module types: `resources`.
    pub(super) textures: bool,
    /// Module types: `data` or `client_data`.
    pub(super) behaviours: bool,
    /// Module types: `client_data`.
    pub(super) scripts: bool,
    /// The archive holds a `level.dat`.
    pub(super) world_template: bool,
}

/// Finds and reads the manifest among `entries`.
pub(super) fn read(entries: &[Entry<'_>]) -> Result<Manifest> {
    let root = entries.iter().find(|e| e.name == MANIFEST);
    // PMMP: the shortest name matching `#.*/manifest.json$#`, the first of equal length.
    let nested = || {
        entries
            .iter()
            .filter(|e| e.name.ends_with(NESTED_MANIFEST))
            .fold(None::<&Entry<'_>>, |best, e| match best {
                Some(b) if b.name.len() <= e.name.len() => Some(b),
                _ => Some(e),
            })
    };
    let Some(entry) = root.or_else(nested) else {
        return Err(if entries.iter().any(|e| e.name == OLD_MANIFEST) {
            Error::OldFormat
        } else {
            Error::ManifestMissing
        });
    };
    if usize::try_from(entry.size).map_or(true, |size| size > MAX_MANIFEST_BYTES) {
        return Err(Error::ManifestTooLarge {
            limit: MAX_MANIFEST_BYTES,
        });
    }
    let bytes = zip::read(entry).map_err(Error::Archive)?;
    let mut manifest = parse(&bytes)?;
    // gophertunnel: a `level.dat` anywhere makes the pack a world template.
    manifest.world_template = entries
        .iter()
        .any(|e| e.name == LEVEL_DAT || e.name.ends_with(b"/level.dat"));
    Ok(manifest)
}

/// The fields of `manifest.json` Anchor reads; others are ignored.
#[derive(Deserialize)]
struct Json {
    /// Required (PMMP), not used.
    #[serde(rename = "format_version")]
    _format_version: i64,
    header: Header,
    modules: Vec<Module>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    uuid: String,
    version: Version,
}

#[derive(Deserialize)]
struct Module {
    #[serde(rename = "type")]
    kind: String,
    /// Required (PMMP), not used.
    #[serde(rename = "uuid")]
    _uuid: String,
    /// Required (PMMP), not used.
    #[serde(rename = "version")]
    _version: Version,
}

/// `[1, 0, 0]` or `"1.0.0"`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Version {
    Numbers(Vec<u32>),
    Text(String),
}

impl Version {
    /// `x.y.z` with the numbers in plain decimal, or `None` unless there are exactly three.
    fn canonical(&self) -> Option<String> {
        let numbers: Vec<u32> = match self {
            Self::Numbers(numbers) => numbers.clone(),
            Self::Text(text) => text
                .split('.')
                .map(|part| {
                    let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
                    digits.then(|| part.parse().ok()).flatten()
                })
                .collect::<Option<_>>()?,
        };
        match numbers.as_slice() {
            [major, minor, patch] => Some(format!("{major}.{minor}.{patch}")),
            _ => None,
        }
    }
}

/// Parses a manifest's bytes.
fn parse(bytes: &[u8]) -> Result<Manifest> {
    let text = std::str::from_utf8(bytes).map_err(|e| Error::Manifest(Box::new(e)))?;
    let json: Json =
        serde_json::from_str(&strip(text)).map_err(|e| Error::Manifest(Box::new(e)))?;
    let uuid = crate::login::parse_uuid(&json.header.uuid).ok_or(Error::Uuid)?;
    let version = json.header.version.canonical().ok_or(Error::Version)?;
    let has = |types: &[&str]| {
        json.modules
            .iter()
            .any(|m| types.contains(&m.kind.as_str()))
    };
    Ok(Manifest {
        name: json.header.name,
        uuid,
        version,
        textures: has(&["resources"]),
        behaviours: has(&["data", "client_data"]),
        scripts: has(&["client_data"]),
        world_template: false,
    })
}

/// `text` without `//` and `/* */` comments and without commas before `}` or `]`, outside
/// strings (what `Ahc\Json\Comment` strips for PMMP). Comment ends keep their newline.
fn strip(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut string = false;
    while let Some(&b) = bytes.get(i) {
        let next = bytes.get(i + 1).copied();
        if string {
            out.push(b);
            if b == b'\\' {
                out.extend(next);
                i += 1;
            } else if b == b'"' {
                string = false;
            }
        } else {
            match (b, next) {
                (b'/', Some(b'/')) => {
                    while bytes.get(i).is_some_and(|&c| c != b'\n') {
                        i += 1;
                    }
                    continue;
                }
                (b'/', Some(b'*')) => {
                    i += 2;
                    while i < bytes.len() && !ends_comment(bytes, i) {
                        i += 1;
                    }
                    i += 2;
                    continue;
                }
                (b',', _) if closes_next(bytes, i + 1) => {}
                (b'"', _) => {
                    string = true;
                    out.push(b);
                }
                _ => out.push(b),
            }
        }
        i += 1;
    }
    // Only ASCII bytes were removed, between characters, so the rest is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Whether a `*/` starts at `at`.
fn ends_comment(bytes: &[u8], at: usize) -> bool {
    bytes.get(at..).is_some_and(|rest| rest.starts_with(b"*/"))
}

/// Whether the next byte from `at` that is not whitespace or inside a comment closes an object or
/// an array.
fn closes_next(bytes: &[u8], mut at: usize) -> bool {
    loop {
        match (bytes.get(at), bytes.get(at + 1)) {
            (Some(b' ' | b'\t' | b'\r' | b'\n'), _) => at += 1,
            (Some(b'/'), Some(b'/')) => {
                while bytes.get(at).is_some_and(|&c| c != b'\n') {
                    at += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                at += 2;
                while at < bytes.len() && !ends_comment(bytes, at) {
                    at += 1;
                }
                at += 2;
            }
            (Some(b'}' | b']'), _) => return true,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::tests::UUID;
    use crate::pack::zip::tests::{File, archive};

    fn manifest(text: &str) -> Result<Manifest> {
        parse(text.as_bytes())
    }

    fn sample(version: &str, modules: &str) -> String {
        format!(
            r#"{{"format_version": 2, "header": {{"name": "Test", "uuid": "{UUID}",
            "version": {version}}}, "modules": [{modules}]}}"#
        )
    }

    const RESOURCES: &str = r#"{"type": "resources", "uuid": "x", "version": [1, 0, 0]}"#;

    #[test]
    fn a_resource_manifest_parses() {
        let m = manifest(&sample("[1, 2, 3]", RESOURCES)).unwrap();
        assert_eq!(m.name, "Test");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.uuid, crate::login::parse_uuid(UUID).unwrap());
        assert!(m.textures && !m.behaviours && !m.scripts);
    }

    #[test]
    fn text_versions_are_read_and_bad_versions_refused() {
        let m = manifest(&sample(r#""01.0.20""#, RESOURCES)).unwrap();
        assert_eq!(m.version, "1.0.20");
        for bad in [
            r#""1.0""#,
            r#""1.0.0.0""#,
            r#""1.-1.0""#,
            r#""1. 0.0""#,
            "[1, 0]",
            "[1, 2, 3, 4]",
        ] {
            assert!(
                matches!(manifest(&sample(bad, RESOURCES)), Err(Error::Version)),
                "{bad}"
            );
        }
        assert!(matches!(
            manifest(&sample("[1, -1, 0]", RESOURCES)),
            Err(Error::Manifest(_))
        ));
    }

    #[test]
    fn missing_required_fields_are_refused() {
        let no_modules = format!(
            r#"{{"format_version": 2, "header": {{"name": "a", "uuid": "{UUID}", "version": [1, 0, 0]}}}}"#
        );
        assert!(matches!(manifest(&no_modules), Err(Error::Manifest(_))));
        let no_format = format!(
            r#"{{"header": {{"name": "a", "uuid": "{UUID}", "version": [1, 0, 0]}}, "modules": []}}"#
        );
        assert!(matches!(manifest(&no_format), Err(Error::Manifest(_))));
        let module_without_uuid = sample("[1, 0, 0]", r#"{"type": "data", "version": [1, 0, 0]}"#);
        assert!(matches!(
            manifest(&module_without_uuid),
            Err(Error::Manifest(_))
        ));
        let bad_uuid = sample("[1, 0, 0]", RESOURCES).replace(UUID, "not-a-uuid");
        assert!(matches!(manifest(&bad_uuid), Err(Error::Uuid)));
        assert!(matches!(manifest("[]"), Err(Error::Manifest(_))));
        assert!(matches!(parse(b"\xff"), Err(Error::Manifest(_))));
    }

    #[test]
    fn module_types_decide_behaviours_and_scripts() {
        let data = r#"{"type": "data", "uuid": "x", "version": [1, 0, 0]}"#;
        let client = r#"{"type": "client_data", "uuid": "y", "version": "1.0.0"}"#;
        let m = manifest(&sample("[1, 0, 0]", &format!("{RESOURCES}, {data}"))).unwrap();
        assert!(m.textures && m.behaviours && !m.scripts);
        let m = manifest(&sample("[1, 0, 0]", client)).unwrap();
        assert!(!m.textures && m.behaviours && m.scripts);
    }

    #[test]
    fn comments_and_trailing_commas_are_stripped_outside_strings() {
        let text = format!(
            "// pack\n{{\"format_version\": 2, /* the header */ \"header\": {{\"name\": \"a // b /* c */\",\n\
             \"uuid\": \"{UUID}\", \"version\": [1, 0, 0,],}}, // end\n\"modules\": [{RESOURCES}, /* x */ ],\n}}"
        );
        let m = manifest(&text).unwrap();
        assert_eq!(m.name, "a // b /* c */");
        assert_eq!(strip(r#"["a,]", "\"", 1,]"#), r#"["a,]", "\"", 1]"#);
        assert_eq!(strip("[1, // x\n]"), "[1 \n]");
        assert_eq!(strip("/* never closed"), "");
    }

    fn files(list: &[(&str, &[u8])]) -> Vec<u8> {
        let files: Vec<File<'_>> = list
            .iter()
            .map(|&(name, data)| File {
                name,
                data,
                deflate: true,
            })
            .collect();
        archive(&files)
    }

    fn read_zip(zip: &[u8]) -> Result<Manifest> {
        read(&zip::entries(zip).unwrap())
    }

    #[test]
    fn the_root_manifest_wins_then_the_shortest_nested_one() {
        let good = sample("[1, 0, 0]", RESOURCES);
        let other = sample("[2, 0, 0]", RESOURCES);
        let (good, other) = (good.as_bytes(), other.as_bytes());
        let zip = files(&[("deep/er/manifest.json", other), ("manifest.json", good)]);
        assert_eq!(read_zip(&zip).unwrap().version, "1.0.0");
        let zip = files(&[
            ("pack/sub/manifest.json", other),
            ("pack/manifest.json", good),
            ("pac2/manifest.json", other),
        ]);
        assert_eq!(read_zip(&zip).unwrap().version, "1.0.0");
        let zip = files(&[("xmanifest.json", good)]);
        assert!(matches!(read_zip(&zip), Err(Error::ManifestMissing)));
        let zip = files(&[("pack_manifest.json", good)]);
        assert!(matches!(read_zip(&zip), Err(Error::OldFormat)));
        let zip = files(&[("manifest.json", good), ("world/level.dat", b"")]);
        assert!(read_zip(&zip).unwrap().world_template);
    }

    #[test]
    fn an_oversized_manifest_is_refused_before_it_is_read() {
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        let zip = files(&[("manifest.json", &big)]);
        assert!(matches!(
            read_zip(&zip),
            Err(Error::ManifestTooLarge { .. })
        ));
    }
}
