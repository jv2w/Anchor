//! Chunk keys and records on disk: key layouts, versions, paletted and legacy sub-chunks, legacy
//! extra data, extras and their caps.

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

/// A biome storage on disk (header low bit set, as BDS writes it).
pub(crate) fn biome_storage(bits: u8, indices: &[u16], palette: &[u32]) -> Vec<u8> {
    let mut out = vec![bits << 1 | 1];
    for word in pack(bits, indices) {
        out.extend_from_slice(&word.to_le_bytes());
    }
    if bits != 0 {
        out.extend_from_slice(&u32::try_from(palette.len()).unwrap().to_le_bytes());
    }
    for id in palette {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

/// A `Data3D` value: a zero height map and `storages`.
pub(crate) fn data_3d(storages: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0; HEIGHT_MAP];
    for storage in storages {
        out.extend_from_slice(storage);
    }
    out
}

/// 24 storages: one uniform biome, then 23 copies.
pub(crate) fn uniform_3d(id: u32) -> Vec<u8> {
    let mut storages = vec![biome_storage(0, &[], &[id])];
    storages.extend(std::iter::repeat_n(vec![0xff], SUB_CHUNKS - 1));
    data_3d(&storages)
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

/// The error inside the chunk error.
fn inner(result: Result<Raw>) -> Error {
    match result {
        Err(Error::Chunk {
            x: 3,
            z: -2,
            source,
        }) => *source,
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
        overworld(&[DATA_3D]),
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
    assert_eq!(classify(&digest_key(5, 6)), Key::Actor);
    assert_eq!(classify(&actor_key([1; 8])), Key::Actor);
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
    assert_eq!(raw.biomes, Biomes::Missing);
}

#[test]
fn chunk_data_over_64_mib_is_refused() {
    let mut e = Entries::new(0, 0);
    e.add(&key(0, 0, &[BLOCK_ENTITIES]), &vec![0; MAX_CHUNK_BYTES / 2])
        .unwrap();
    let error = e.add_actor(vec![0; MAX_CHUNK_BYTES / 2 + 1]).unwrap_err();
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
}

#[test]
fn data_3d_biomes_decode_with_copies_and_extra_storages() {
    let varied = biome_storage(
        2,
        &(0..4096_u16).map(|i| i % 3).collect::<Vec<_>>(),
        &[1, 4, 7],
    );
    let mut storages = vec![varied.clone(), vec![0xff]];
    storages.extend((0..22).map(|i| biome_storage(0, &[], &[i])));
    let raw = decode(&entries(42, &[(&[DATA_3D], data_3d(&storages))])).unwrap();
    let Biomes::Paletted(biomes) = raw.biomes else {
        panic!("not paletted")
    };
    assert_eq!(biomes.len(), SUB_CHUNKS);
    assert_eq!(biomes[0], biomes[1]);
    assert_eq!(biomes[0].palette, vec![1, 4, 7]);
    assert_eq!(biomes[23].palette, vec![21]);
    // Version 37 stored 32 storages; the 8 after the 24 used are read and dropped.
    let mut many = storages.clone();
    many.extend(std::iter::repeat_n(vec![0xff], 8));
    let raw = decode(&entries(37, &[(&[DATA_3D], data_3d(&many))])).unwrap();
    assert!(matches!(raw.biomes, Biomes::Paletted(b) if b.len() == SUB_CHUNKS));
    // Version 37 data that stops after 24 is accepted too (PMMP).
    assert!(decode(&entries(37, &[(&[DATA_3D], data_3d(&storages))])).is_ok());
    // But storages past the version's count are trailing data.
    assert!(matches!(
        inner(decode(&entries(42, &[(&[DATA_3D], data_3d(&many))]))),
        Error::Malformed("Data3D continues after its biomes")
    ));
    assert!(matches!(
        inner(decode(&entries(
            42,
            &[(&[DATA_3D], data_3d(&storages[..23]))]
        ))),
        Error::Malformed(_)
    ));
    assert!(matches!(
        inner(decode(&entries(
            42,
            &[(&[DATA_3D], data_3d(&[vec![0xff]]))]
        ))),
        Error::Malformed("first biome storage copies the one before")
    ));
    assert!(matches!(
        inner(decode(&entries(30, &[(&[DATA_3D], uniform_3d(1))]))),
        Error::Malformed("chunk version has no 3D biomes")
    ));
    assert!(matches!(
        inner(decode(&entries(42, &[(&[DATA_3D], vec![0; 100])]))),
        Error::Malformed(_)
    ));
}

#[test]
fn data_2d_biomes_win_over_data_3d_like_pmmp() {
    let mut columns = vec![0; HEIGHT_MAP];
    columns.extend((0..256).map(|i| u8::try_from(i).unwrap()));
    let raw = decode(&entries(
        20,
        &[(&[DATA_2D], columns.clone()), (&[DATA_3D], uniform_3d(2))],
    ))
    .unwrap();
    let Biomes::Columns(ids) = &raw.biomes else {
        panic!("not columns")
    };
    assert_eq!(ids[0x12], 0x12);
    assert_eq!(raw.ignored, vec![DATA_3D]);
    columns.pop();
    assert!(matches!(
        inner(decode(&entries(20, &[(&[DATA_2D], columns)]))),
        Error::Malformed(_)
    ));
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
    for i in 0..256_u32 {
        terrain.extend_from_slice(&((i % 40) << 24 | 0x00ab_cdef).to_be_bytes());
    }
    let e = entries(
        2,
        &[
            (&[LEGACY_TERRAIN], terrain.clone()),
            (&[SUB_CHUNK, 0], vec![0]),
            (&[DATA_2D], vec![]),
        ],
    );
    let raw = decode(&e).unwrap();
    assert_eq!(raw.sections.len(), 8);
    assert_eq!(raw.ignored, vec![DATA_2D, SUB_CHUNK]);
    let Layer::Legacy(layer) = &raw.sections[3].layers[0] else {
        panic!("not legacy")
    };
    for ((x, y, z), golden) in LEGACY_COLUMN_GOLDEN {
        let (id, meta) = layer.get(x, y, z);
        assert_eq!(u16::from(id) << 4 | u16::from(meta), golden, "{x} {y} {z}");
    }
    let Biomes::Columns(ids) = &raw.biomes else {
        panic!("not columns")
    };
    assert_eq!(ids[45], 5);
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
fn block_entities_entities_and_pending_ticks_are_nbt() {
    let mut chests = nbt(&compound("Chest"));
    chests.extend(nbt(&compound("Sign")));
    let ticks = Compound::from([("currentTick".to_owned(), Value::Int(9))]);
    let e = entries(
        42,
        &[
            (&[BLOCK_ENTITIES], chests),
            (&[ENTITIES], nbt(&compound("Pig"))),
            (&[PENDING_TICKS], nbt(&ticks)),
        ],
    );
    let raw = decode(&e).unwrap();
    assert_eq!(
        raw.block_entities,
        vec![compound("Chest"), compound("Sign")]
    );
    assert_eq!(raw.entities, vec![compound("Pig")]);
    assert_eq!(raw.pending_ticks, Some(ticks.clone()));
    // With a digp key the actors are the entities, and tag 50 is old.
    let mut digest = e.clone();
    digest.set_digest();
    digest.add_actor(nbt(&compound("Cow"))).unwrap();
    let raw = decode(&digest).unwrap();
    assert_eq!(raw.entities, vec![compound("Cow")]);
    assert_eq!(raw.ignored, vec![ENTITIES]);
    let mut trailing = nbt(&ticks);
    trailing.push(0);
    assert!(matches!(
        inner(decode(&entries(42, &[(&[PENDING_TICKS], trailing)]))),
        Error::Malformed("value continues after its compound")
    ));
    assert!(matches!(
        inner(decode(&entries(42, &[(&[ENTITIES], vec![10, 0])]))),
        Error::Nbt(_)
    ));
}

#[test]
fn lists_over_the_extra_entry_cap_are_refused() {
    let one = nbt(&Compound::new());
    let many = one.repeat(MAX_EXTRA_ENTRIES + 1);
    assert!(matches!(
        inner(decode(&entries(42, &[(&[BLOCK_ENTITIES], many)]))),
        Error::TooLarge { .. }
    ));
    assert!(matches!(actor_ids(&[0; 9]), Err(Error::Malformed(_))));
    assert!(matches!(
        actor_ids(&vec![0; 8 * (MAX_EXTRA_ENTRIES + 1)]),
        Err(Error::TooLarge { .. })
    ));
    assert_eq!(actor_ids(&[1; 16]).unwrap(), &[[1; 8], [1; 8]]);
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
fn mutated_sub_chunks_data_3d_and_nbt_never_panic() {
    let water = block("minecraft:water", &[("liquid_depth", Value::Int(0))]);
    fuzz_value(
        &[SUB_CHUNK, 1],
        &sub_chunk_v9(1, &[stone_layer(), layer(0, &[], &[water])]),
    );
    let varied = biome_storage(
        2,
        &(0..4096_u16).map(|i| i % 3).collect::<Vec<_>>(),
        &[1, 4, 7],
    );
    let mut storages = vec![varied, vec![0xff]];
    storages.extend((0..22).map(|i| biome_storage(0, &[], &[i])));
    fuzz_value(&[DATA_3D], &data_3d(&storages));
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
