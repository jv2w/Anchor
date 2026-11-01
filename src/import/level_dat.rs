//! `level.dat`: the world's settings (PMMP `BedrockWorldData`).
//!
//! Layout: little-endian `i32` storage version, `i32` length of the rest, then one little-endian
//! NBT root compound filling exactly that length. Like PMMP, the version that counts is the
//! `StorageVersion` tag (BDS 1.26.52 writes 10 in both places), a world newer than 10 is refused,
//! and `NetworkVersion` must be at most the protocol Anchor speaks (BDS 1.26.52 writes 2193, the
//! protocol of the same release): a newer game may have stored blocks Anchor cannot read.

use super::{Error, Result};
use crate::nbt::{Compound, Encoding, Value};
use crate::protocol::Reader;
use crate::protocol::packet::PROTOCOL_VERSION;
// Decoding `level.dat` allocates within the data files' own budget.
use crate::storage::seal::DECODE_BUDGET;

/// The newest `StorageVersion` read (PMMP `WorldDataVersions::STORAGE`).
pub(crate) const STORAGE_VERSION: i32 = 10;
/// The `SpawnY` Bedrock writes before the spawn has a height: the game puts it on the ground.
pub(crate) const SPAWN_Y_UNSET: i32 = 32_767;

/// What Anchor reads of `level.dat`: the spawn and the time, which are state. No setting (seed,
/// game mode, difficulty, game rules) is read: the imported world runs with the server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorldData {
    /// `SpawnX`, `SpawnY`, `SpawnZ`; a `SpawnY` of [`SPAWN_Y_UNSET`] has no height yet.
    pub(crate) spawn: [i32; 3],
    /// `Time`: the time of day in ticks.
    pub(crate) time: i64,
}

/// Reads `level.dat`.
pub(crate) fn parse(bytes: &[u8]) -> Result<WorldData> {
    let (header, body) = bytes
        .split_first_chunk::<8>()
        .ok_or(Error::Malformed("level.dat shorter than its header"))?;
    let [_, _, _, _, l0, l1, l2, l3] = *header;
    let length = u32::from_le_bytes([l0, l1, l2, l3]);
    if usize::try_from(length).ok() != Some(body.len()) {
        return Err(Error::Malformed("level.dat length differs from its header"));
    }
    let mut r = Reader::new(body, DECODE_BUDGET, 0);
    let root = r.nbt(Encoding::LittleEndian).map_err(Error::Nbt)?;
    if !r.remaining().is_empty() {
        return Err(Error::Malformed("level.dat continues after its compound"));
    }
    let storage_version = int(
        &root,
        "StorageVersion",
        "level.dat has no int StorageVersion",
    )?;
    if storage_version > STORAGE_VERSION {
        return Err(Error::NewerFormat {
            what: "level.dat StorageVersion",
            version: storage_version.into(),
            supported: STORAGE_VERSION.into(),
        });
    }
    let network_version = int(
        &root,
        "NetworkVersion",
        "level.dat has no int NetworkVersion",
    )?;
    if network_version > PROTOCOL_VERSION {
        return Err(Error::NewerFormat {
            what: "level.dat NetworkVersion",
            version: network_version.into(),
            supported: PROTOCOL_VERSION.into(),
        });
    }
    Ok(WorldData {
        spawn: [
            int(&root, "SpawnX", "level.dat has no int SpawnX")?,
            int(&root, "SpawnY", "level.dat has no int SpawnY")?,
            int(&root, "SpawnZ", "level.dat has no int SpawnZ")?,
        ],
        time: long(&root, "Time", "level.dat has no long Time")?,
    })
}

fn int(root: &Compound, name: &str, missing: &'static str) -> Result<i32> {
    match root.get(name) {
        Some(&Value::Int(value)) => Ok(value),
        _ => Err(Error::Malformed(missing)),
    }
}

fn long(root: &Compound, name: &str, missing: &'static str) -> Result<i64> {
    match root.get(name) {
        Some(&Value::Long(value)) => Ok(value),
        _ => Err(Error::Malformed(missing)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::Writer;

    /// A `level.dat` holding `root`.
    pub(crate) fn encode(root: &Compound) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        w.nbt(root, Encoding::LittleEndian).unwrap();
        let body = w.into_inner();
        let mut out = STORAGE_VERSION.to_le_bytes().to_vec();
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub(crate) fn root() -> Compound {
        Compound::from([
            ("StorageVersion".to_owned(), Value::Int(10)),
            ("NetworkVersion".to_owned(), Value::Int(PROTOCOL_VERSION)),
            ("SpawnX".to_owned(), Value::Int(4)),
            ("SpawnY".to_owned(), Value::Int(SPAWN_Y_UNSET)),
            ("SpawnZ".to_owned(), Value::Int(-8)),
            ("RandomSeed".to_owned(), Value::Long(-77)),
            ("Time".to_owned(), Value::Long(6000)),
            ("LevelName".to_owned(), Value::String("w".to_owned())),
        ])
    }

    #[test]
    fn reads_the_fields_anchor_imports() {
        let world = parse(&encode(&root())).unwrap();
        assert_eq!(world.spawn, [4, SPAWN_Y_UNSET, -8]);
        assert_eq!(world.time, 6000);
        // The seed is a setting: not read, so not needed.
        let mut no_seed = root();
        no_seed.remove("RandomSeed");
        assert_eq!(parse(&encode(&no_seed)).unwrap(), world);
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "off-tick: test fixtures on disk")]
    fn bds_fixture_level_dat_reads() {
        let bytes =
            std::fs::read(std::path::Path::new(crate::import::tests::FIXTURE).join("level.dat"))
                .unwrap();
        let world = parse(&bytes).unwrap();
        assert_eq!(world.spawn, [0, SPAWN_Y_UNSET, 0]);
    }

    #[test]
    fn newer_worlds_and_missing_fields_are_refused() {
        let mut newer = root();
        newer.insert("StorageVersion".to_owned(), Value::Int(11));
        assert!(matches!(
            parse(&encode(&newer)),
            Err(Error::NewerFormat { version: 11, .. })
        ));
        let mut protocol = root();
        protocol.insert(
            "NetworkVersion".to_owned(),
            Value::Int(PROTOCOL_VERSION + 1),
        );
        assert!(matches!(
            parse(&encode(&protocol)),
            Err(Error::NewerFormat { .. })
        ));
        for field in [
            "StorageVersion",
            "NetworkVersion",
            "SpawnX",
            "SpawnY",
            "SpawnZ",
            "Time",
        ] {
            let mut missing = root();
            missing.remove(field);
            assert!(
                matches!(parse(&encode(&missing)), Err(Error::Malformed(_))),
                "{field}"
            );
            let mut wrong = root();
            wrong.insert(field.to_owned(), Value::Short(1));
            assert!(
                matches!(parse(&encode(&wrong)), Err(Error::Malformed(_))),
                "{field}"
            );
        }
    }

    #[test]
    fn length_and_trailing_bytes_are_checked() {
        let good = encode(&root());
        assert!(parse(&good[..7]).is_err());
        let mut short = good.clone();
        short.pop();
        assert!(matches!(parse(&short), Err(Error::Malformed(_))));
        let mut trailing = good.clone();
        trailing.push(0);
        trailing[4] += 1;
        assert!(matches!(parse(&trailing), Err(Error::Malformed(_))));
    }

    #[test]
    fn mutated_level_dat_never_panics() {
        let good = encode(&root());
        crate::protocol::fuzz(&good, 10_000, |data| {
            parse(data)
                .map(|_| ())
                .map_err(|_| crate::protocol::Error::Size)
        });
    }
}
