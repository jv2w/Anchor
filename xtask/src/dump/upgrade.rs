//! The upgrade part of `cargo xtask dump`: copies the block and item upgrade
//! schemas from their pinned checkouts into `xtask/data/upgrade/`, converts the legacy block table
//! `id_meta_to_nbt/1.12.0.bin` into typed JSON lines, records the SHA-256 of every source file,
//! and runs PMMP's own upgrader over the copies (`xtask/php/upgrade.php`) for the golden files the
//! import's tests compare with.
//!
//! The block schemas are opencollab-incubator/BedrockBlockUpgradeSchema 5.4.1, not the 5.2.0
//! PMMP's composer pins (it lacks the changes from 1.26.30 on). The item schemas are
//! opencollab-incubator/BedrockItemUpgradeSchema 1.19.0, newer than PMMP's 1.17.0: it adds the
//! 1.26.50 rename (`photo` to `photo_item`) and the 1.26.60 frostbite spawn egg.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PHP, PMMP, REFERENCE_BLOCKS, REFERENCE_DIR, create_dir, pinned, read, write};

/// The pinned checkouts, relative to the repository root, and their commits.
pub(super) const BLOCKS: (&str, &str) = (
    ".cache/block-upgrade-schema",
    "1077ee640ed39f60266f7a7c8ab331ef76d7931e",
);
pub(super) const ITEMS: (&str, &str) = (
    ".cache/item-upgrade-schema",
    "dfc12b388be886ba6980d9a3b67cb1b432b563e2",
);
/// The output directory, relative to the repository root.
pub(crate) const OUT: &str = "xtask/data/upgrade";
/// Output names inside [`OUT`].
pub(crate) const BLOCK_SCHEMAS: &str = "blocks";
pub(crate) const ITEM_SCHEMAS: &str = "items";
pub(crate) const BLOCK_LEGACY_IDS: &str = "block_legacy_id_map.json";
pub(crate) const LEGACY_TABLE: &str = "legacy_1.12.0.jsonl";
pub(crate) const ITEM_LEGACY_IDS: &str = "item_legacy_id_map.json";
pub(crate) const ITEM_BLOCKS: &str = "1.12.0_item_id_to_block_id_map.json";
const SOURCES: &str = "sources.json";
pub(crate) const GOLDEN: &str = "golden";
pub(crate) const GOLDEN_FILES: [&str; 3] = ["legacy.jsonl", "states.jsonl", "reference.jsonl"];
/// Source folders of the schemas.
const BLOCK_SCHEMA_DIR: &str = "nbt_upgrade_schema";
const ITEM_SCHEMA_DIR: &str = "id_meta_upgrade_schema";
/// The legacy block tables: 1.12.0 is the one PMMP loads; 1.9.0 only feeds the golden run.
const LEGACY_BIN: &str = "id_meta_to_nbt/1.12.0.bin";
const LEGACY_BIN_1_9: &str = "id_meta_to_nbt/1.9.0.bin";

/// Writes everything into `out` (the committed folder, or a scratch one for `--check`) and
/// returns the files written, relative to `out`.
pub(super) fn run(root: &Path, out: &Path) -> Result<Vec<String>, String> {
    for (dir, commit) in [BLOCKS, ITEMS] {
        pinned(root, dir, commit)?;
    }
    let blocks = root.join(BLOCKS.0);
    let items = root.join(ITEMS.0);
    for dir in [
        out.to_path_buf(),
        out.join(BLOCK_SCHEMAS),
        out.join(ITEM_SCHEMAS),
        out.join(GOLDEN),
    ] {
        create_dir(&dir)?;
    }
    // (source file, file written into `out`), the converted table aside.
    let mut copies: Vec<(PathBuf, String, String)> = Vec::new();
    for name in json_files(&blocks.join(BLOCK_SCHEMA_DIR))? {
        copies.push((
            blocks.clone(),
            format!("{BLOCK_SCHEMA_DIR}/{name}"),
            format!("{BLOCK_SCHEMAS}/{name}"),
        ));
    }
    copies.push((
        blocks.clone(),
        BLOCK_LEGACY_IDS.to_owned(),
        BLOCK_LEGACY_IDS.to_owned(),
    ));
    for name in json_files(&items.join(ITEM_SCHEMA_DIR))? {
        copies.push((
            items.clone(),
            format!("{ITEM_SCHEMA_DIR}/{name}"),
            format!("{ITEM_SCHEMAS}/{name}"),
        ));
    }
    for file in [ITEM_LEGACY_IDS, ITEM_BLOCKS] {
        copies.push((
            items.clone(),
            file.to_owned(),
            format!("{ITEM_SCHEMAS}/{file}"),
        ));
    }

    let mut sources = serde_json::Map::new();
    sources.insert("block".to_owned(), BLOCKS.1.into());
    sources.insert("item".to_owned(), ITEMS.1.into());
    let mut written = Vec::with_capacity(copies.len() + 2 + GOLDEN_FILES.len());
    let repo = |dir: &Path| if dir == blocks { "block" } else { "item" };
    for (dir, source, to) in &copies {
        let bytes = read(&dir.join(source))?;
        sources.insert(
            format!("{}/{source}", repo(dir)),
            hex(&sha256(&bytes)).into(),
        );
        // The repository stores text with LF (`.gitattributes`); the item schemas come with CR LF.
        let text = String::from_utf8(bytes).map_err(|e| format!("{source}: {e}"))?;
        write(&out.join(to), text.replace("\r\n", "\n").as_bytes())?;
        written.push(to.clone());
    }
    for bin in [LEGACY_BIN, LEGACY_BIN_1_9] {
        let bytes = read(&blocks.join(bin))?;
        sources.insert(format!("block/{bin}"), hex(&sha256(&bytes)).into());
        if bin == LEGACY_BIN {
            let lines = legacy_lines(&bytes).map_err(|e| format!("{bin}: {e}"))?;
            write(&out.join(LEGACY_TABLE), lines.as_bytes())?;
            written.push(LEGACY_TABLE.to_owned());
        }
    }
    let mut text = serde_json::to_string_pretty(&sources).map_err(|e| format!("{SOURCES}: {e}"))?;
    text.push('\n');
    write(&out.join(SOURCES), text.as_bytes())?;
    written.push(SOURCES.to_owned());

    let status = Command::new(root.join(PHP))
        .args(["-d", "memory_limit=2G"])
        .arg(root.join("xtask").join("php").join("upgrade.php"))
        .arg(out)
        .arg(blocks.join(LEGACY_BIN))
        .arg(blocks.join(LEGACY_BIN_1_9))
        .arg(root.join(REFERENCE_DIR).join(REFERENCE_BLOCKS))
        .current_dir(root.join(PMMP.0))
        .status()
        .map_err(|e| format!("cannot run {PHP} ({e}); see .claude/rules/data.md"))?;
    if !status.success() {
        return Err(format!("the PMMP upgrade golden run failed ({status})"));
    }
    written.extend(GOLDEN_FILES.iter().map(|f| format!("{GOLDEN}/{f}")));
    Ok(written)
}

/// The `.json` files directly in `dir`, sorted.
#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: xtask is a development tool"
)]
fn json_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if Path::new(&name).extension() == Some("json".as_ref()) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The legacy table (PMMP `BlockIdMetaUpgrader::loadFromString` format: unsigned varint ID
/// count; per ID its varint-length name and varint meta count; per meta the varint meta and a
/// little-endian NBT state) as one JSON line per entry, in file order:
/// `{"id":..,"meta":..,"state":{"name":..,"states":{..},"version":..}}`, values typed as in the
/// schemas. Fails on trailing bytes, as PMMP does.
fn legacy_lines(bin: &[u8]) -> Result<String, String> {
    let mut r = Bin { bytes: bin, at: 0 };
    let mut out = String::new();
    let ids = r.varint()?;
    for _ in 0..ids {
        let len = r.varint()?;
        let id = String::from_utf8(r.take(len)?.to_vec()).map_err(|e| e.to_string())?;
        let metas = r.varint()?;
        for _ in 0..metas {
            let meta = r.varint()?;
            let state = r.root()?;
            out.push_str(
                &serde_json::to_string(&serde_json::json!({
                    "id": id,
                    "meta": meta,
                    "state": state,
                }))
                .map_err(|e| e.to_string())?,
            );
            out.push('\n');
        }
    }
    if r.at != bin.len() {
        return Err("trailing bytes after the last entry".to_owned());
    }
    Ok(out)
}

/// A reader of the legacy table's bytes.
struct Bin<'a> {
    bytes: &'a [u8],
    at: usize,
}

/// NBT tag types a block state holds.
const TAG_END: u8 = 0;
const TAG_BYTE: u8 = 1;
const TAG_SHORT: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_STRING: u8 = 8;
const TAG_COMPOUND: u8 = 10;

impl Bin<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).ok_or("length overflows")?;
        let bytes = self.bytes.get(self.at..end).ok_or("unexpected end")?;
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?.first().copied().unwrap_or_default())
    }

    fn varint(&mut self) -> Result<usize, String> {
        let mut value = 0usize;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            value |= usize::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint too long".to_owned())
    }

    fn string(&mut self) -> Result<String, String> {
        let len = u16::from_le_bytes(self.array()?);
        String::from_utf8(self.take(usize::from(len))?.to_vec()).map_err(|e| e.to_string())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "short read".to_owned())
    }

    /// A root compound (type, empty name, body) as typed JSON: a compound becomes an object
    /// whose values are `{"byte": ..}`, `{"short": ..}`, `{"int": ..}` or `{"string": ..}`, the
    /// `states` compound nested the same way and `name`/`version` plain.
    fn root(&mut self) -> Result<serde_json::Value, String> {
        if self.byte()? != TAG_COMPOUND {
            return Err("a state is not a compound".to_owned());
        }
        self.string()?;
        let mut state = serde_json::Map::new();
        loop {
            let tag = self.byte()?;
            if tag == TAG_END {
                return Ok(serde_json::Value::Object(state));
            }
            let key = self.string()?;
            let value = match (key.as_str(), tag) {
                ("name", TAG_STRING) => serde_json::Value::String(self.string()?),
                ("version", TAG_INT) => i32::from_le_bytes(self.array()?).into(),
                ("states", TAG_COMPOUND) => self.states()?,
                _ => return Err(format!("unexpected state key {key} of type {tag}")),
            };
            state.insert(key, value);
        }
    }

    fn states(&mut self) -> Result<serde_json::Value, String> {
        let mut states = serde_json::Map::new();
        loop {
            let tag = self.byte()?;
            if tag == TAG_END {
                return Ok(serde_json::Value::Object(states));
            }
            let key = self.string()?;
            let value = match tag {
                TAG_BYTE => serde_json::json!({ "byte": self.byte()? }),
                TAG_SHORT => serde_json::json!({ "short": i16::from_le_bytes(self.array()?) }),
                TAG_INT => serde_json::json!({ "int": i32::from_le_bytes(self.array()?) }),
                TAG_STRING => serde_json::json!({ "string": self.string()? }),
                _ => return Err(format!("unexpected property {key} of type {tag}")),
            };
            states.insert(key, value);
        }
    }
}

/// Lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|b| [b >> 4, b & 15])
        .filter_map(|n| char::from_digit(u32::from(n), 16))
        .collect()
}

/// The SHA-256 round constants (FIPS 180-4 4.2.2).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];
/// The SHA-256 initial hash value (FIPS 180-4 5.3.3).
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// SHA-256 (FIPS 180-4) of `data`: the dump records the hash of every source file and
/// `vendor-test` checks its downloads, and the tool has no hashing dependency.
#[expect(
    clippy::indexing_slicing,
    reason = "invariant: the schedule indices are 0..64 with fixed offsets into a 64-word array"
)]
#[expect(
    clippy::many_single_char_names,
    reason = "consistency: the working variables keep FIPS 180-4's names"
)]
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = H0;
    let mut message = data.to_vec();
    let bits = u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    let (blocks, _) = message.as_chunks::<64>();
    for block in blocks {
        let mut w = [0u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (word, bytes) in w.iter_mut().zip(words) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let (w15, w2) = (w[i - 15], w[i - 2]);
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (k, wi) in K.iter().zip(w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0; 32];
    let (bytes, _) = out.as_chunks_mut::<4>();
    for (bytes, word) in bytes.iter_mut().zip(h) {
        *bytes = word.to_be_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_fips_180_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_legacy_table_converts_to_typed_lines_and_trailing_bytes_fail() {
        // One ID ("a", 1 byte), one meta (3): {name: "a", states: {b: 1b, i: 2, s: "x"}, version: 7}.
        let mut bin = vec![1, 1, b'a', 1, 3, TAG_COMPOUND, 0, 0];
        let put_str = |bin: &mut Vec<u8>, s: &str| {
            bin.extend_from_slice(&u16::try_from(s.len()).unwrap().to_le_bytes());
            bin.extend_from_slice(s.as_bytes());
        };
        bin.push(TAG_STRING);
        put_str(&mut bin, "name");
        put_str(&mut bin, "a");
        bin.push(TAG_COMPOUND);
        put_str(&mut bin, "states");
        bin.push(TAG_BYTE);
        put_str(&mut bin, "b");
        bin.push(1);
        bin.push(TAG_INT);
        put_str(&mut bin, "i");
        bin.extend_from_slice(&2i32.to_le_bytes());
        bin.push(TAG_STRING);
        put_str(&mut bin, "s");
        put_str(&mut bin, "x");
        bin.push(TAG_END);
        bin.push(TAG_INT);
        put_str(&mut bin, "version");
        bin.extend_from_slice(&7i32.to_le_bytes());
        bin.push(TAG_END);
        assert_eq!(
            legacy_lines(&bin).unwrap(),
            "{\"id\":\"a\",\"meta\":3,\"state\":{\"name\":\"a\",\"states\":{\"b\":{\"byte\":1},\
             \"i\":{\"int\":2},\"s\":{\"string\":\"x\"}},\"version\":7}}\n"
        );
        bin.push(0);
        assert!(legacy_lines(&bin).is_err());
    }
}
