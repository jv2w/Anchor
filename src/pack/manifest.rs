//! `manifest.json` of a pack: which archive entry is the manifest, and what Anchor reads of it.
//! The rules are in the module documentation of [`pack`](super).

use serde::Deserialize;

use super::zip::{self, Entry};
use super::{Error, Result};
use crate::protocol::Uuid;

/// The name of a world template's level file.
const LEVEL_DAT: &[u8] = b"level.dat";

/// The name of the manifest file.
const MANIFEST: &[u8] = b"manifest.json";

/// The largest `manifest.json` read; real ones are a few KiB.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// The end of a level file's name below a folder.
const NESTED_LEVEL_DAT: &[u8] = b"/level.dat";

/// The end of a manifest's name below a folder.
const NESTED_MANIFEST: &[u8] = b"/manifest.json";

/// The name of the old pack format's manifest file.
const OLD_MANIFEST: &[u8] = b"pack_manifest.json";

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
    pub(super) behaviors: bool,
    /// Module types: `client_data`.
    pub(super) scripts: bool,
    /// The archive holds a `level.dat`.
    pub(super) world_template: bool,
}

/// The fields of `manifest.json` Anchor reads; others are ignored.
#[derive(Deserialize)]
struct Json {
    /// Required, not used.
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
    /// Required, not used.
    #[serde(rename = "uuid")]
    _uuid: String,
    /// Required, not used.
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

/// The manifest among `entries`, found and read.
pub(super) fn read(entries: &[Entry<'_>]) -> Result<Manifest> {
    let root = entries.iter().find(|e| e.name == MANIFEST);
    // The shortest name matching `#.*/manifest.json$#`, the first of equal length.
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
        } else if entries.iter().any(is_backslash_manifest) {
            Error::Archive(
                "entry names use backslashes; pack the folder's contents with a tool that \
                 writes `/`",
            )
        } else {
            Error::ManifestMissing
        });
    };
    if usize::try_from(entry.size).map_or(true, |size| size > MAX_MANIFEST_BYTES) {
        return Err(Error::ManifestTooLarge {
            limit: MAX_MANIFEST_BYTES,
        });
    }
    let bytes = zip::read(entry)?;
    let mut manifest = parse(&bytes)?;
    // gophertunnel: a `level.dat` anywhere makes the pack a world template.
    manifest.world_template = entries
        .iter()
        .any(|e| e.name == LEVEL_DAT || e.name.ends_with(NESTED_LEVEL_DAT));
    Ok(manifest)
}

/// Whether `entry` is a `manifest.json` below a folder whose name has a backslash in it, which
/// the archive reader accepts as a separator and the manifest search does not.
fn is_backslash_manifest(entry: &Entry<'_>) -> bool {
    entry.name.contains(&b'\\')
        && entry.name.rsplit(|&b| zip::is_separator(b)).next() == Some(MANIFEST)
}

/// The manifest that the bytes of `manifest.json` hold.
fn parse(bytes: &[u8]) -> Result<Manifest> {
    let json: Json =
        serde_json::from_slice(&strip(bytes)).map_err(|e| Error::Manifest(Box::new(e)))?;
    let uuid = json.header.uuid.parse().ok().ok_or(Error::Uuid)?;
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
        behaviors: has(&["data", "client_data"]),
        scripts: has(&["client_data"]),
        world_template: false,
    })
}

/// `bytes` without `//` and `/* */` comments and without commas before `}` or `]`, outside
/// strings. Each removed byte becomes a space, and a
/// removed line break stays, so a syntax error is reported where the file has it.
fn strip(bytes: &[u8]) -> Vec<u8> {
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
                        out.push(b' ');
                        i += 1;
                    }
                    continue;
                }
                (b'/', Some(b'*')) => {
                    let start = i;
                    i += 2;
                    while i < bytes.len() && !ends_comment(bytes, i) {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                    out.extend(
                        bytes
                            .iter()
                            .skip(start)
                            .take(i - start)
                            .map(|&c| if c == b'\n' { c } else { b' ' }),
                    );
                    continue;
                }
                (b',', _) if closes_next(bytes, i + 1) => out.push(b' '),
                (b'"', _) => {
                    string = true;
                    out.push(b);
                }
                _ => out.push(b),
            }
        }
        i += 1;
    }
    out
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

    /// A `resources` module entry of a manifest.
    const RESOURCES: &str = r#"{"type": "resources", "uuid": "x", "version": [1, 0, 0]}"#;

    fn manifest(text: &str) -> Result<Manifest> {
        parse(text.as_bytes())
    }

    fn sample(version: &str, modules: &str) -> String {
        format!(
            r#"{{"format_version": 2, "header": {{"name": "Test", "uuid": "{UUID}",
            "version": {version}}}, "modules": [{modules}]}}"#
        )
    }

    #[test]
    fn a_resource_manifest_parses() {
        let m = manifest(&sample("[1, 2, 3]", RESOURCES)).unwrap();
        assert_eq!(m.name, "Test");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.uuid, UUID.parse().unwrap());
        assert!(m.textures && !m.behaviors && !m.scripts);
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
    fn module_types_decide_behaviors_and_scripts() {
        let data = r#"{"type": "data", "uuid": "x", "version": [1, 0, 0]}"#;
        let client = r#"{"type": "client_data", "uuid": "y", "version": "1.0.0"}"#;
        let m = manifest(&sample("[1, 0, 0]", &format!("{RESOURCES}, {data}"))).unwrap();
        assert!(m.textures && m.behaviors && !m.scripts);
        let m = manifest(&sample("[1, 0, 0]", client)).unwrap();
        assert!(!m.textures && m.behaviors && m.scripts);
    }

    #[test]
    fn comments_and_trailing_commas_are_stripped_outside_strings() {
        let text = format!(
            "// pack\n{{\"format_version\": 2, /* the header */ \"header\": {{\"name\": \"a // b /* c */\",\n\
             \"uuid\": \"{UUID}\", \"version\": [1, 0, 0,],}}, // end\n\"modules\": [{RESOURCES}, /* x */ ],\n}}"
        );
        let m = manifest(&text).unwrap();
        assert_eq!(m.name, "a // b /* c */");
        assert_eq!(strip(br#"["a,]", "\"", 1,]"#), br#"["a,]", "\"", 1 ]"#);
        assert_eq!(strip(b"[1, // x\n]"), b"[1      \n]");
        assert_eq!(strip(b"/* never closed"), b" ".repeat(15));
    }

    #[test]
    fn a_syntax_error_is_reported_at_its_line_after_comments() {
        let Err(Error::Manifest(source)) = manifest("{\n/* one\ntwo\nthree */\n]") else {
            panic!("the manifest is malformed");
        };
        assert!(source.to_string().contains("line 5"), "{source}");
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
    fn a_manifest_under_a_backslash_name_says_so() {
        let good = sample("[1, 0, 0]", RESOURCES);
        let zip = files(&[("MyPack\\manifest.json", good.as_bytes())]);
        let Err(Error::Archive(what)) = read_zip(&zip) else {
            panic!("the manifest is not found");
        };
        assert!(what.contains("backslashes"), "{what}");
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
