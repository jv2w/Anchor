//! Chunk keys and records on disk: key layouts, versions, paletted and legacy sub-chunks, legacy
//! extra data, block entities and their caps, the tags not read, and errors naming the key.

use super::*;
use crate::nbt::Value;
use crate::protocol::Writer;
use crate::storage::codec::tests::{block, pack};

/// Little-endian NBT bytes of `compound`.
pub(crate) fn nbt(compound: &Compound) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), 0);
    w.nbt(compound, Encoding::LittleEndian).unwrap();
    w.into_inner()
}

/// A block layer on disk.
pub(crate) fn layer(bits: u8, indices: &[u16], palette: &[Compound]) -> Vec<u8> {
    let mut out = vec![bits << 1];
    for word in pack(bits, indices) {
        out.extend_from_slice(&word.to_le_bytes());
    }
    if bits != 0 {
        out.extend_from_slice(&i32::try_from(palette.len()).unwrap().to_le_bytes());
    }
    for entry in palette {
        out.extend(nbt(entry));
    }
    out
}

/// A version 9 sub-chunk at `y` with the given layers.
pub(crate) fn sub_chunk_v9(y: i8, layers: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![9, u8::try_from(layers.len()).unwrap(), y.to_le_bytes()[0]];
    for l in layers {
        out.extend_from_slice(l);
    }
    out
}

/// The low byte of `n`.
fn byte(n: usize) -> u8 {
    u8::try_from(n & 0xff).unwrap()
}

fn stone_layer() -> Vec<u8> {
    let indices: Vec<u16> = (0..4096).map(|i| u16::from(i % 7 == 0)).collect();
    layer(
        1,
        &indices,
        &[block("minecraft:air", &[]), block("minecraft:stone", &[])],
    )
}

fn key(x: i32, z: i32, suffix: &[u8]) -> Vec<u8> {
    let mut key = prefix(x, z).to_vec();
    key.extend_from_slice(suffix);
    key
}

fn entries(version: u8, values: &[(&[u8], Vec<u8>)]) -> Entries {
    let mut e = Entries::new(3, -2);
    e.add(&key(3, -2, &[VERSION]), &[version]).unwrap();
    for (suffix, value) in values {
        e.add(&key(3, -2, suffix), value).unwrap();
    }
    e
}

/// The error inside the chunk error, and inside the key error if there is one.
fn inner(result: Result<Raw>) -> Error {
    match result {
        Err(Error::Chunk {
            x: 3,
            z: -2,
            source,
        }) => match *source {
            Error::Key { source, .. } => *source,
            other => other,
        },
        other => panic!("{other:?}"),
    }
}

/// The kind and hexadecimal bytes of the key the chunk error names.
fn faulty_key(result: Result<Raw>) -> (&'static str, String) {
    match result {
        Err(Error::Chunk { source, .. }) => match *source {
            Error::Key { kind, key, .. } => (kind, key),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn keys_are_classified_by_their_layout() {
    let overworld = |suffix: &[u8]| classify(&key(-1, 2, suffix));
    assert_eq!(
        overworld(&[VERSION]),
        Key::Overworld {
            x: -1,
            z: 2,
            version: true
        }
    );
    assert_eq!(
        overworld(&[OLD_VERSION]),
        Key::Overworld {
            x: -1,
            z: 2,
            version: true
        }
    );
    assert_eq!(
        overworld(&[0x2b]),
        Key::Overworld {
            x: -1,
            z: 2,
            version: false
        }
    );
    assert_eq!(
        overworld(&[SUB_CHUNK, 0xfc]),
        Key::Overworld {
            x: -1,
            z: 2,
            version: false
        }
    );
    assert_eq!(
        overworld(&[1, 0, 0, 0, VERSION]),
        Key::Dimension {
            dimension: 1,
            version: true
        }
    );
    assert_eq!(
        overworld(&[2, 0, 0, 0, SUB_CHUNK, 3]),
        Key::Dimension {
            dimension: 2,
            version: false
        }
    );
    assert_eq!(
        classify(&[&b"digp"[..], &prefix(5, 6)].concat()),
        Key::EntityList
    );
    assert_eq!(
        classify(&[&b"actorprefix"[..], &[1; 8]].concat()),
        Key::Entity
    );
    assert_eq!(classify(b"~local_player"), Key::Player);
    assert_eq!(classify(b"player_server_1234"), Key::Player);
    for other in [
        &b"BiomeData"[..],
        b"Overworld",
        b"mobevents",
        b"scoreboard",
        b"LevelChunkMetaDataDictionary",
        b"AutonomousEntities",
    ] {
        assert_eq!(
            classify(other),
            Key::Other,
            "{}",
            String::from_utf8_lossy(other)
        );
    }
}

#[test]
fn prefix_ranges_end_after_every_key_of_the_chunk() {
    assert_eq!(after(&[1, 2, 0xff]), Some(vec![1, 3]));
    assert_eq!(after(&[0xff, 0xff]), None);
    let p = prefix(-1, -1);
    assert_eq!(p, [0xff; 8]);
    assert_eq!(after(&p), None);
    assert_eq!(prefix(1, 2), [1, 0, 0, 0, 2, 0, 0, 0]);
}

#[test]
fn entries_keep_what_is_decoded_and_note_other_tags() {
    let mut e = Entries::new(3, -2);
    e.add(&key(3, -2, &[0x36]), &[2, 0, 0, 0]).unwrap();
    e.add(&key(3, -2, &[SUB_CHUNK, 24]), &[9]).unwrap();
    e.add(&key(3, -2, &[SUB_CHUNK, 0xfb]), &[9]).unwrap();
    e.add(&key(3, -2, &[1, 0, 0, 0, VERSION]), &[42]).unwrap();
    e.add(&key(3, -2, &[0x3f, 1]), &[]).unwrap();
    e.add(&key(3, -2, &[VERSION]), &[42]).unwrap();
    let raw = decode(&e).unwrap();
    assert_eq!(raw.ignored, vec![SUB_CHUNK, 0x36, 0x3f]);
    assert!(raw.sections.is_empty());
}

#[test]
fn chunk_data_over_64_mib_is_refused() {
    let mut e = Entries::new(0, 0);
    e.add(&key(0, 0, &[BLOCK_ENTITIES]), &vec![0; MAX_CHUNK_BYTES / 2])
        .unwrap();
    let error = e
        .add(
            &key(0, 0, &[SUB_CHUNK, 0]),
            &vec![0; MAX_CHUNK_BYTES / 2 + 1],
        )
        .unwrap_err();
    assert!(matches!(
        error,
        Error::TooLarge {
            what: "chunk data",
            ..
        }
    ));
}

#[test]
fn versions_are_checked() {
    let both = entries(40, &[(&[OLD_VERSION], vec![3])]);
    let raw = decode(&both).unwrap();
    assert_eq!(raw.version, 40);
    assert_eq!(raw.ignored, vec![OLD_VERSION]);
    let mut old = Entries::new(3, -2);
    old.add(&key(3, -2, &[OLD_VERSION]), &[7]).unwrap();
    assert_eq!(decode(&old).unwrap().version, 7);
    assert!(matches!(
        inner(decode(&Entries::new(3, -2))),
        Error::Malformed("chunk has no version")
    ));
    let mut long = Entries::new(3, -2);
    long.add(&key(3, -2, &[VERSION]), &[42, 0]).unwrap();
    assert!(matches!(inner(decode(&long)), Error::Malformed(_)));
    assert_eq!(
        faulty_key(decode(&long)),
        ("Version", "03000000feffffff2c".to_owned())
    );
    let mut old_long = Entries::new(3, -2);
    old_long.add(&key(3, -2, &[OLD_VERSION]), &[]).unwrap();
    assert_eq!(faulty_key(decode(&old_long)).0, "LegacyVersion");
    assert!(matches!(
        inner(decode(&entries(43, &[]))),
        Error::NewerFormat {
            version: 43,
            supported: 42,
            ..
        }
    ));
}

#[test]
fn paletted_sub_chunks_decode_as_stored() {
    let water = block("minecraft:water", &[("liquid_depth", Value::Int(0))]);
    let layer1 = layer(0, &[], std::slice::from_ref(&water));
    let v9 = sub_chunk_v9(-4, &[stone_layer(), layer1]);
    let mut v8 = vec![8, 1];
    v8.extend(stone_layer());
    let mut v1 = vec![1];
    v1.extend(stone_layer());
    let e = entries(
        42,
        &[
            (&[SUB_CHUNK, 0xfc], v9),
            (&[SUB_CHUNK, 0], v8),
            (&[SUB_CHUNK, 19], v1),
        ],
    );
    let raw = decode(&e).unwrap();
    let ys: Vec<(i8, u8, usize)> = raw
        .sections
        .iter()
        .map(|s| (s.y, s.version, s.layers.len()))
        .collect();
    assert_eq!(ys, vec![(-4, 9, 2), (0, 8, 1), (19, 1, 1)]);
    let Layer::Paletted(first) = &raw.sections[0].layers[0] else {
        panic!("not paletted")
    };
    assert_eq!(first.bits, 1);
    assert_eq!(first.palette[1], block("minecraft:stone", &[]));
    let indices: Vec<u16> = (0..4096).map(|i| u16::from(i % 7 == 0)).collect();
    assert_eq!(first.words, pack(1, &indices));
    let Layer::Paletted(second) = &raw.sections[0].layers[1] else {
        panic!("not paletted")
    };
    assert_eq!((second.bits, &second.palette), (0, &vec![water]));
}

#[test]
fn legacy_sub_chunks_keep_ids_and_meta() {
    let mut body = vec![0];
    body.extend((0..4096).map(|i| u8::try_from(i % 256).unwrap()));
    body.extend((0..2048).map(|i| u8::try_from(i % 256).unwrap()));
    let mut with_light = body.clone();
    with_light.extend(vec![0xff; 4096]);
    let mut v3 = body.clone();
    v3[0] = 3;
    let e = entries(
        10,
        &[
            (&[SUB_CHUNK, 0], body.clone()),
            (&[SUB_CHUNK, 1], with_light),
            (&[SUB_CHUNK, 2], v3),
        ],
    );
    let raw = decode(&e).unwrap();
    assert_eq!(raw.sections.len(), 3);
    for section in &raw.sections {
        let Layer::Legacy(layer) = &section.layers[0] else {
            panic!("not legacy")
        };
        assert_eq!(layer.ids[300], 44);
        // Index 3 is the high nibble of byte 1.
        assert_eq!(layer.get(0, 3, 0), (3, 0));
        assert_eq!(layer.get(0, 2, 0), (2, 1));
    }
    let mut short = body;
    short.pop();
    assert!(matches!(
        inner(decode(&entries(10, &[(&[SUB_CHUNK, 0], short)]))),
        Error::Malformed(_)
    ));
}

#[test]
fn caves_and_cliffs_chunk_versions_shift_sub_chunk_keys() {
    for version in 24..=26 {
        let e = entries(
            version,
            &[
                (&[SUB_CHUNK, 0], sub_chunk_v9(0, &[stone_layer()])),
                (&[SUB_CHUNK, 23], sub_chunk_v9(23, &[stone_layer()])),
            ],
        );
        let raw = decode(&e).unwrap();
        let ys: Vec<i8> = raw.sections.iter().map(|s| s.y).collect();
        assert_eq!(ys, vec![-4, 19], "version {version}");
    }
    let e = entries(
        27,
        &[(&[SUB_CHUNK, 23], sub_chunk_v9(23, &[stone_layer()]))],
    );
    let raw = decode(&e).unwrap();
    assert!(raw.sections.is_empty());
    assert_eq!(raw.ignored, vec![SUB_CHUNK]);
}

#[test]
fn malformed_sub_chunks_are_errors() {
    let bad = |value: Vec<u8>| inner(decode(&entries(42, &[(&[SUB_CHUNK, 1], value)])));
    assert!(matches!(
        bad(Vec::new()),
        Error::Malformed("empty sub-chunk")
    ));
    assert!(matches!(
        bad(sub_chunk_v9(2, &[stone_layer()])),
        Error::Malformed("sub-chunk y differs from its key")
    ));
    assert!(matches!(
        bad(vec![10, 0]),
        Error::NewerFormat { version: 10, .. }
    ));
    let mut trailing = sub_chunk_v9(1, &[stone_layer()]);
    trailing.push(0);
    assert!(matches!(
        bad(trailing),
        Error::Malformed("sub-chunk continues after its layers")
    ));
    let mut network = stone_layer();
    network[0] |= 1;
    assert!(matches!(
        bad(sub_chunk_v9(1, &[network])),
        Error::Malformed("block layer holds network ids")
    ));
    let mut seven = stone_layer();
    seven[0] = 7 << 1;
    assert!(matches!(
        bad(sub_chunk_v9(1, &[seven])),
        Error::Malformed("bits per index")
    ));
    let air = block("minecraft:air", &[]);
    let empty_palette = layer(1, &[0; 4096], &[]);
    assert!(matches!(
        bad(sub_chunk_v9(1, &[empty_palette])),
        Error::Malformed("palette length")
    ));
    let over = layer(1, &[0; 4096], &[air.clone(), air.clone(), air.clone()]);
    assert!(matches!(
        bad(sub_chunk_v9(1, &[over])),
        Error::Malformed("palette length")
    ));
    let outside = layer(2, &[3; 4096], &[air.clone(), air]);
    assert!(matches!(
        bad(sub_chunk_v9(1, &[outside])),
        Error::Malformed("block index outside the palette")
    ));
    let mut huge_count = vec![9, 255, 1];
    huge_count.extend(stone_layer());
    assert!(bad(huge_count).to_string().contains("ends early"));
    // The error names the key, with the sub-chunk's y.
    assert_eq!(
        faulty_key(decode(&entries(42, &[(&[SUB_CHUNK, 0xfd], vec![])]))),
        ("SubChunkPrefix", "03000000feffffff2ffd".to_owned())
    );
}

#[test]
fn biomes_entities_and_pending_ticks_are_not_read() {
    // Values each of which failed the chunk when it was read: biomes after the storages the
    // chunk version has (a real 1.21 chunk, U11), a short Data2D, entities and pending ticks that
    // are not NBT.
    let mut data_3d = vec![0; 512];
    data_3d.extend(std::iter::repeat_n(1_u8, 25));
    let e = entries(
        41,
        &[
            (&[0x2b], data_3d),
            (&[0x2d], vec![0; 3]),
            (&[0x32], vec![10, 0]),
            (&[0x33], vec![0xff]),
        ],
    );
    let raw = decode(&e).unwrap();
    assert_eq!(raw.ignored, vec![0x2b, 0x2d, 0x32, 0x33]);
}

/// The legacy terrain golden values: PMMP's `SubChunkConverter::convertSubChunkFromLegacyColumn`
/// (ext-chunkutils2, run under the PM5 PHP build) on ids `(i * 7 + 3) & 0xff` and meta bytes
/// `(i * 13 + 5) & 0xff`, sub-chunk 3, returns `id << 4 | meta` at (x, y, z).
const LEGACY_COLUMN_GOLDEN: [((usize, usize, usize), u16); 5] = [
    ((0, 0, 0), 1341),
    ((1, 2, 3), 3610),
    ((15, 15, 15), 965),
    ((5, 9, 12), 2343),
    ((8, 0, 7), 3389),
];

#[test]
fn legacy_terrain_splits_the_column_like_pmmp() {
    let mut terrain: Vec<u8> = (0..LEGACY_IDS).map(|i| byte(i * 7 + 3)).collect();
    terrain.extend((0..LEGACY_META).map(|i| byte(i * 13 + 5)));
    terrain.extend(vec![0; LEGACY_LIGHT + 256]);
    terrain.extend(vec![0; 1024]);
    let e = entries(
        2,
        &[
            (&[LEGACY_TERRAIN], terrain.clone()),
            (&[SUB_CHUNK, 0], vec![0]),
            (&[0x2d], vec![]),
        ],
    );
    let raw = decode(&e).unwrap();
    assert_eq!(raw.sections.len(), 8);
    assert_eq!(raw.ignored, vec![0x2d, SUB_CHUNK]);
    let Layer::Legacy(layer) = &raw.sections[3].layers[0] else {
        panic!("not legacy")
    };
    for ((x, y, z), golden) in LEGACY_COLUMN_GOLDEN {
        let (id, meta) = layer.get(x, y, z);
        assert_eq!(u16::from(id) << 4 | u16::from(meta), golden, "{x} {y} {z}");
    }
    terrain.pop();
    assert!(matches!(
        inner(decode(&entries(2, &[(&[LEGACY_TERRAIN], terrain)]))),
        Error::Malformed(_)
    ));
    assert!(matches!(
        inner(decode(&entries(1, &[]))),
        Error::Malformed("chunk has no legacy terrain")
    ));
}

/// The legacy sub-chunk golden values: PMMP's `SubChunkConverter::convertSubChunkXZY` on the
/// first 4,096 ids and 2,048 meta bytes of the column above.
const LEGACY_XZY_GOLDEN: [((usize, usize, usize), u16); 5] = [
    ((0, 0, 0), 53),
    ((1, 2, 3), 1562),
    ((15, 15, 15), 4047),
    ((5, 9, 12), 2089),
    ((8, 0, 7), 317),
];

#[test]
fn legacy_sub_chunks_index_blocks_like_pmmp() {
    let mut value = vec![0];
    value.extend((0..4096_usize).map(|i| byte(i * 7 + 3)));
    value.extend((0..2048_usize).map(|i| byte(i * 13 + 5)));
    let raw = decode(&entries(5, &[(&[SUB_CHUNK, 0], value)])).unwrap();
    let Layer::Legacy(layer) = &raw.sections[0].layers[0] else {
        panic!("not legacy")
    };
    for ((x, y, z), golden) in LEGACY_XZY_GOLDEN {
        let (id, meta) = layer.get(x, y, z);
        assert_eq!(u16::from(id) << 4 | u16::from(meta), golden, "{x} {y} {z}");
    }
}

#[test]
fn legacy_extra_data_unpacks_positions_by_chunk_version() {
    let mut value = 2_i32.to_le_bytes().to_vec();
    value.extend_from_slice(&(3 << 12 | 5 << 8 | 0xc8_u32).to_le_bytes());
    value.extend_from_slice(&(31_u16 | 2 << 8).to_le_bytes());
    value.extend_from_slice(&(3 << 11 | 5 << 7 | 0x64_u32).to_le_bytes());
    value.extend_from_slice(&(0xaf_u16 | 0x1f << 8).to_le_bytes());
    let new = decode(&entries(3, &[(&[LEGACY_EXTRA], value.clone())])).unwrap();
    assert_eq!(
        new.legacy_blocks[0],
        LegacyBlock {
            x: 3,
            y: 200,
            z: 5,
            id: 31,
            meta: 2
        }
    );
    assert_eq!(new.legacy_blocks[1].meta, 0xf);
    let terrain = vec![0; LEGACY_TERRAIN_LEN];
    let old = decode(&entries(
        2,
        &[
            (&[LEGACY_EXTRA], value.clone()),
            (&[LEGACY_TERRAIN], terrain),
        ],
    ))
    .unwrap();
    assert_eq!(
        old.legacy_blocks[1],
        LegacyBlock {
            x: 3,
            y: 100,
            z: 5,
            id: 175,
            meta: 0xf
        }
    );
    assert!(
        decode(&entries(3, &[(&[LEGACY_EXTRA], Vec::new())]))
            .unwrap()
            .legacy_blocks
            .is_empty()
    );
    let mut long = value;
    long.push(0);
    assert!(matches!(
        inner(decode(&entries(3, &[(&[LEGACY_EXTRA], long)]))),
        Error::Malformed(_)
    ));
    let negative = (-1_i32).to_le_bytes().to_vec();
    assert!(matches!(
        inner(decode(&entries(3, &[(&[LEGACY_EXTRA], negative)]))),
        Error::Malformed(_)
    ));
}

fn compound(id: &str) -> Compound {
    Compound::from([("id".to_owned(), Value::String(id.to_owned()))])
}

#[test]
fn block_entities_are_nbt() {
    let mut chests = nbt(&compound("Chest"));
    chests.extend(nbt(&compound("Sign")));
    let raw = decode(&entries(42, &[(&[BLOCK_ENTITIES], chests)])).unwrap();
    assert_eq!(
        raw.block_entities,
        vec![compound("Chest"), compound("Sign")]
    );
    assert!(
        decode(&entries(42, &[(&[BLOCK_ENTITIES], Vec::new())]))
            .unwrap()
            .block_entities
            .is_empty()
    );
    let bad = entries(42, &[(&[BLOCK_ENTITIES], vec![10, 0])]);
    assert!(matches!(inner(decode(&bad)), Error::Nbt(_)));
    assert_eq!(faulty_key(decode(&bad)).0, "BlockEntity");
}

#[test]
fn lists_over_the_extra_entry_cap_are_refused() {
    let one = nbt(&Compound::new());
    let many = one.repeat(MAX_EXTRA_ENTRIES + 1);
    assert!(matches!(
        inner(decode(&entries(42, &[(&[BLOCK_ENTITIES], many)]))),
        Error::TooLarge { .. }
    ));
}

fn fuzz_value(suffix: &'static [u8], seed: &[u8]) {
    let decoded = crate::protocol::fuzz(seed, 10_000, |data| {
        decode(&entries(42, &[(suffix, data.to_vec())]))
            .map(|_| ())
            .map_err(|_| crate::protocol::Error::Size)
    });
    assert!(decoded > 0);
}

#[test]
fn mutated_sub_chunks_and_nbt_never_panic() {
    let water = block("minecraft:water", &[("liquid_depth", Value::Int(0))]);
    fuzz_value(
        &[SUB_CHUNK, 1],
        &sub_chunk_v9(1, &[stone_layer(), layer(0, &[], &[water])]),
    );
    let mut list = nbt(&Compound::from([
        ("id".to_owned(), Value::String("Chest".to_owned())),
        (
            "Items".to_owned(),
            Value::List(crate::nbt::List::Compound(vec![
                compound("a"),
                compound("b"),
            ])),
        ),
        ("x".to_owned(), Value::Int(4)),
    ]));
    list.extend(nbt(&compound("Sign")));
    fuzz_value(&[BLOCK_ENTITIES], &list);
}
