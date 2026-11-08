//! `cargo xtask p6 import`: the world import against the official server. It starts the official
//! server copy on a new world with the official server's own terrain (seed [`SEED`]) and spawn
//! radius 0, lets the spawn bot record the sub-chunks the official server sends
//! (`target/p6/import-official/spawn.world.jsonl`), stops the official server, which saves the
//! world, and copies the world folder to `target/p6/import-world/` (the official server never runs
//! from it). It then imports that folder into Anchor's default world (a Void world of the same
//! name) with `Server::import_world`, starts Anchor, lets the bot record the sub-chunks Anchor
//! sends (`import-anchor/spawn.world.jsonl`), and compares them block by block: every block of
//! every sub-chunk both servers answered (an all-air answer holds air), in every layer (a layer one
//! side does not send is air), must have the same network ID.
//!
//! Anchor keeps a block the dump does not have as an unknown block. It sends the stored state when
//! the client knows it (an inert block, with no server behaviour) and `info_update` otherwise: a
//! block the official server sends where Anchor sends `info_update`, or a different state where
//! Anchor sends the stored one (the official server updates some blocks after loading), is an
//! intended difference when its name is listed in `xtask/data/p6/intended.txt` as
//! `import | block <name> | <reason>`. A block the official server sends that Anchor sends as the
//! state the dump source reads it as is intended too: the dump source ignores some vanilla
//! properties, such as a pumpkin's facing (`xtask/data/dump/deserialized.json`). Every other
//! difference fails, printed as the pair of states with how many blocks and where.
//!
//! Then the export: Anchor's world is written back as a Bedrock world with
//! `Server::export_world` (`target/p6/export-world/`), the official server starts on a copy of it
//! with the same game rules, the bot records what the official server sends
//! (`export-official/spawn.world.jsonl`), and that is compared with what the official server sent
//! of its own world in the same way: the official server loads the export as the world it saved,
//! except where Anchor stores a block as the dump source reads it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anchor::protocol::consts::SubChunkResult;
use serde_json::Value;

use super::api::{INTENDED, parse_intended};
use super::compare::field;
use super::{
    OUT_DIR, OfficialSession, anchor_config, block_states, build_bot, install_log, official_exe,
    official_session_with, run_anchor, run_bot, state_text,
};
use crate::fs::{copy_dir, create_dir, read_text, remove_dir};
use crate::r#gen::data::Data;
use crate::{say, status};

/// Blocks in a sub-chunk layer.
const BLOCKS: usize = 4096;
/// Differing blocks shown per pair of states.
const EXAMPLES: usize = 3;
/// Game rules set before the bot joins: the player spawns on the world spawn, as Anchor puts
/// it, and nothing changes blocks between the official server sending them and saving them but
/// what the world's own pending updates do (fire burning vines next to lava).
const RULES: [(&str, &str); 4] = [
    ("spawnradius", "0"),
    ("dofiretick", "false"),
    ("randomtickspeed", "0"),
    ("domobspawning", "false"),
];
/// The scenario's name in `intended.txt`.
pub(super) const SCENARIO: &str = "import";
/// The seed of the official server's world.
const SEED: &str = "20261004";
/// The bits in one word of a block storage.
const WORD_BITS: usize = 32;
/// The world's name, in the official server and in Anchor.
const WORLD: &str = "p6-import";

/// How many blocks of a pair of states differ, and where the first [`EXAMPLES`] are: x, y, z and
/// layer.
type Differing = (usize, Vec<[i64; 4]>);

/// The layers of one sent sub-chunk, each its 4,096 block network IDs in storage order (x, z,
/// y).
type Layers = Vec<Vec<u32>>;

/// Runs the scenario; returns whether it passed.
pub(super) fn command(root: &Path, out: &mut impl Write) -> Result<bool, String> {
    let exe = official_exe(root)?;
    let bot = build_bot(root)?;
    let states = block_states(root)?;
    let intended = parse_intended(&read_text(&root.join(INTENDED))?)
        .map_err(|e| format!("{INTENDED}: {e}"))?;
    let aliases = aliases(root)?;
    let dir = |name: &str| root.join(OUT_DIR).join(name);
    let (official_dir, world_dir, anchor_dir) = (
        dir("import-official"),
        dir("import-world"),
        dir("import-anchor"),
    );
    let (export_dir, export_official_dir) = (dir("export-world"), dir("export-official"));

    let mut passed = official_run(&exe, &bot, &official_dir, |_| Ok(()), out)?;
    let saved = exe
        .parent()
        .ok_or("the official server copy has no folder")?
        .join("worlds")
        .join(WORLD);
    remove_dir(&world_dir)?;
    copy_dir(&saved, &world_dir)?;

    passed &= anchor_session(&bot, &world_dir, &anchor_dir, out)?;
    let read = |d: &Path| {
        let path = d.join("spawn.world.jsonl");
        sub_chunks(&read_text(&path)?).map_err(|e| format!("{}: {e}", path.display()))
    };
    passed &= compare(
        &read(&official_dir)?,
        &read(&anchor_dir)?,
        &states,
        intended.blocks(),
        &aliases,
        ["official", "Anchor"],
        out,
    )?;

    remove_dir(&export_dir)?;
    export(&anchor_dir, &export_dir, out)?;
    passed &= official_run(
        &exe,
        &bot,
        &export_official_dir,
        |official| copy_dir(&export_dir, &official.join("worlds").join(WORLD)),
        out,
    )?;
    passed &= compare(
        &read(&official_dir)?,
        &read(&export_official_dir)?,
        &states,
        intended.blocks(),
        &aliases,
        ["official", "official on the export"],
        out,
    )?;
    say(if passed { "PASS" } else { "FAIL" }, out)?;
    Ok(passed)
}

/// Starts the official server on the world [`WORLD`] (a new one of seed [`SEED`], or the one
/// `prepare` puts in the copy's folder), sets the [`RULES`] and runs the spawn bot into
/// `<out_dir>/spawn.jsonl`.
fn official_run(
    exe: &Path,
    bot: &Path,
    out_dir: &Path,
    prepare: impl FnOnce(&Path) -> Result<(), String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let session = OfficialSession {
        transport: "raknet",
        compression: "snappy",
        settings: &[
            ("level-type", "DEFAULT"),
            ("level-seed", SEED),
            ("level-name", WORLD),
        ],
    };
    official_session_with(
        exe,
        out_dir,
        &session,
        prepare,
        |addr, official, out| {
            for (rule, value) in RULES {
                official.set_game_rule(rule, value)?;
            }
            spawn_bot(bot, addr, out_dir, "official", out)
        },
        out,
    )
}

/// The network ID of each reference state that is not an Anchor state but that the dump source
/// reads as one, with that Anchor state's network ID (the generator's aliases).
fn aliases(root: &Path) -> Result<BTreeMap<u32, u32>, String> {
    let data = Data::load(root)?;
    data.aliases
        .iter()
        .map(|a| {
            let state = data
                .states
                .get(usize::from(a.state))
                .ok_or("an alias names no state")?;
            Ok((a.network_id, state.network_id))
        })
        .collect()
}

/// Imports `world` into a fresh Anchor data folder under `out_dir`, starts Anchor on it and runs
/// the spawn bot.
fn anchor_session(
    bot: &Path,
    world: &Path,
    out_dir: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    let data_dir = out_dir.join("data");
    remove_dir(&data_dir)?;
    create_dir(&data_dir)?;
    let server = server(&data_dir)?;
    install_log()?;
    let report = server
        .import_world(WORLD, world)
        .map_err(|e| format!("import: {e} ({:?})", std::error::Error::source(&e)))?;
    say(&format!("import: {report:?}"), out)?;
    let passed = run_anchor(server, |ports| {
        let addr = format!("127.0.0.1:{}", ports.raknet);
        spawn_bot(bot, &addr, out_dir, "Anchor", out)
    })?;
    Ok(passed && report.chunks() > 0)
}

/// Exports the world of the Anchor session in `anchor_dir` into the new folder `folder`.
fn export(anchor_dir: &Path, folder: &Path, out: &mut impl Write) -> Result<(), String> {
    let report = server(&anchor_dir.join("data"))?
        .export_world(WORLD, folder)
        .map_err(|e| format!("export: {e} ({:?})", std::error::Error::source(&e)))?;
    say(&format!("export: {report:?}"), out)
}

/// An Anchor server of the data folder `data_dir` whose default world is [`WORLD`], a Void
/// world.
fn server(data_dir: &Path) -> Result<anchor::Server, String> {
    let mut config = anchor_config();
    WORLD.clone_into(&mut config.general.level_name);
    config.general.level_type = anchor::config::LevelType::Void;
    anchor::Server::builder()
        .config(config)
        .data_dir(data_dir)
        .without_console_log()
        .build()
        .map_err(|e| format!("build Anchor: {e}"))
}

/// Runs the spawn bot against `addr` into `<dir>/spawn.jsonl`.
fn spawn_bot(
    bot: &Path,
    addr: &str,
    dir: &Path,
    server: &str,
    out: &mut impl Write,
) -> Result<bool, String> {
    let capture = dir.join("spawn.jsonl");
    let args = ["-addr", addr, "-transport", "raknet", "-scenario", "spawn"];
    let outcome = run_bot(bot, &args, &capture)?;
    say(
        &format!(
            "{} {server} spawn{}",
            status(outcome.passed),
            outcome.failure()
        ),
        out,
    )?;
    Ok(outcome.passed)
}

/// The sub-chunks of a capture's `<capture>.world.jsonl`, by absolute sub-chunk position, decoded
/// from their payloads (the last answer wins).
fn sub_chunks(world: &str) -> Result<BTreeMap<[i64; 3], Layers>, String> {
    let mut out = BTreeMap::new();
    for (i, line) in world.lines().enumerate() {
        let at = |e: &str| format!("line {}: {e}", i + 1);
        let value: Value = serde_json::from_str(line).map_err(|e| at(&e.to_string()))?;
        if field(&value, "name").as_str() != Some("SubChunk") {
            continue;
        }
        let fields = field(&value, "fields");
        let centre = field(fields, "Position");
        for entry in field(fields, "SubChunkEntries")
            .as_array()
            .into_iter()
            .flatten()
        {
            // An answer without blocks (all air) has no payload; any other answer without one is
            // an error answer and holds nothing to compare.
            let payload = field(entry, "RawPayload").as_str();
            let all_air = field(entry, "Result").as_u64()
                == Some(u64::from(u8::from(SubChunkResult::SuccessAllAir)));
            if payload.is_none() && !all_air {
                continue;
            }
            let offset = field(entry, "Offset");
            let mut position = [0_i64; 3];
            for (k, slot) in position.iter_mut().enumerate() {
                let (Some(c), Some(o)) = (
                    centre.get(k).and_then(Value::as_i64),
                    offset.get(k).and_then(Value::as_i64),
                ) else {
                    return Err(at("a position is not numbers"));
                };
                *slot = c + o;
            }
            let layers = match payload {
                Some(payload) => {
                    layers(&base64(payload).map_err(|e| at(&e))?).map_err(|e| at(&e))?
                }
                None => Layers::new(),
            };
            out.insert(position, layers);
        }
    }
    Ok(out)
}

/// Standard base64 with padding, as Go's JSON writes byte slices.
fn base64(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0_u32, 0_u32);
    for c in text.bytes().filter(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("not base64: {c}")),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).map_err(|e| e.to_string())?);
        }
    }
    Ok(out)
}

/// The block layers of a network sub-chunk payload (version 8 or 9, runtime palettes): each
/// layer's network ID of every block.
fn layers(payload: &[u8]) -> Result<Layers, String> {
    let mut r = payload.iter().copied();
    let mut byte = || r.next().ok_or("the payload ends early");
    let version = byte()?;
    let count = byte()?;
    if version == 9 {
        byte()?;
    } else if version != 8 {
        return Err(format!("sub-chunk version {version}"));
    }
    let mut layers = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let bits = usize::from(byte()? >> 1);
        if !matches!(bits, 0..=6 | 8 | 16) {
            return Err(format!("width {bits}"));
        }
        let mut words = Vec::new();
        // Indices per word; none at width 0.
        let per = WORD_BITS.checked_div(bits);
        if let Some(per) = per {
            for _ in 0..BLOCKS.div_ceil(per) {
                words.push(u32::from_le_bytes([byte()?, byte()?, byte()?, byte()?]));
            }
        }
        let mut varint = || -> Result<i32, String> {
            let mut value = 0_u32;
            for shift in (0..35).step_by(7) {
                let b = byte()?;
                value |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    // Zigzag, as gophertunnel's Varint32.
                    return Ok((value >> 1).cast_signed() ^ -(value & 1).cast_signed());
                }
            }
            Err("a varint is too long".to_owned())
        };
        let len = if per.is_some() { varint()? } else { 1 };
        let palette = (0..len)
            .map(|_| varint().map(i32::cast_unsigned))
            .collect::<Result<Vec<u32>, String>>()?;
        let blocks = (0..BLOCKS)
            .map(|i| {
                let index = per.map_or(0, |per| {
                    let word = words.get(i / per).copied().unwrap_or_default();
                    (word >> ((i % per) * bits)) & ((1 << bits) - 1)
                });
                palette
                    .get(usize::try_from(index).unwrap_or(usize::MAX))
                    .copied()
                    .ok_or_else(|| format!("block {i} indexes past the palette"))
            })
            .collect::<Result<Vec<u32>, String>>()?;
        layers.push(blocks);
    }
    Ok(layers)
}

/// A network ID's state as `name[properties]`.
fn state_name(states: &BTreeMap<u32, String>, id: u32) -> String {
    states
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("network id {id}"))
}

/// Compares, block by block, the sub-chunks both captures hold and prints the result, naming
/// the two sides `sides`: each pair of differing states with how many blocks and the first
/// positions. `official` is what the official server sent of its own world, `anchor` what was
/// sent of Anchor's world (by Anchor, or by the official server from its export). Where Anchor
/// sends `info_update` (an unknown block), or a stored block of an intended name the official
/// server changed after loading, the difference is excused as [`excuse`] says. Where Anchor's
/// world has the state the dump source reads the official server's as (`aliases`), the block is
/// excused too.
fn compare(
    official: &BTreeMap<[i64; 3], Layers>,
    anchor: &BTreeMap<[i64; 3], Layers>,
    states: &BTreeMap<u32, String>,
    intended: &BTreeSet<String>,
    aliases: &BTreeMap<u32, u32>,
    [first, second]: [&str; 2],
    out: &mut impl Write,
) -> Result<bool, String> {
    let id_of = |name: &str| {
        states
            .iter()
            .find(|(_, s)| s.as_str() == name)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("the reference list has no {name}"))
    };
    let (air, unknown) = (
        id_of(&state_text("minecraft:air", ""))?,
        id_of(&state_text("minecraft:info_update", ""))?,
    );
    let name = |id: u32| {
        let state = state_name(states, id);
        state.split('[').next().unwrap_or_default().to_owned()
    };
    // (official server state, Anchor state) → count and the first positions.
    let mut pairs: BTreeMap<(u32, u32), Differing> = BTreeMap::new();
    let mut excused: BTreeMap<String, usize> = BTreeMap::new();
    let mut compared = 0_usize;
    for (at, a) in official {
        let Some(b) = anchor.get(at) else {
            continue;
        };
        compared += 1;
        let block = |l: &Layers, layer: usize, i: usize| {
            l.get(layer).and_then(|l| l.get(i)).copied().unwrap_or(air)
        };
        for i in 0..BLOCKS {
            let (a0, a1, b0, b1) = (
                block(a, 0, i),
                block(a, 1, i),
                block(b, 0, i),
                block(b, 1, i),
            );
            if let Some(why) = excuse([a0, a1], [b0, b1], air, unknown, intended, &name) {
                *excused.entry(why).or_default() += 1;
                continue;
            }
            for layer in 0..a.len().max(b.len()).max(2) {
                let (x, y) = (block(a, layer, i), block(b, layer, i));
                if x == y {
                    continue;
                }
                if aliases.get(&x) == Some(&y) {
                    *excused
                        .entry(format!("{} as the dump source reads it", name(x)))
                        .or_default() += 1;
                    continue;
                }
                let n = i64::try_from(i).map_err(|e| e.to_string())?;
                let pos = [
                    at[0] * 16 + (n >> 8),
                    at[1] * 16 + (n & 15),
                    at[2] * 16 + ((n >> 4) & 15),
                    i64::try_from(layer).map_err(|e| e.to_string())?,
                ];
                let entry = pairs.entry((x, y)).or_default();
                entry.0 += 1;
                if entry.1.len() < EXAMPLES {
                    entry.1.push(pos);
                }
            }
        }
    }
    for ((x, y), (count, examples)) in &pairs {
        say(
            &format!(
                "DIFFERENT {SCENARIO}: {count} blocks {first} {} {second} {}, at (x, y, z, layer) \
                 {examples:?}",
                state_name(states, *x),
                state_name(states, *y)
            ),
            out,
        )?;
    }
    say(
        &format!(
            "{SCENARIO}: {compared} sub-chunks {first} and {second} sent ({} only {first} sent, {} \
             only {second} sent); intended blocks (unknown blocks Anchor sends as info_update, \
             and blocks Anchor keeps as the dump source reads them): {excused:?}",
            official.keys().filter(|k| !anchor.contains_key(*k)).count(),
            anchor.keys().filter(|k| !official.contains_key(*k)).count(),
        ),
        out,
    )?;
    for name in intended.iter().filter(|n| !excused.contains_key(*n)) {
        say(
            &format!("note {SCENARIO}: `block {name}` in {INTENDED} was not seen"),
            out,
        )?;
    }
    Ok(compared > 0 && pairs.is_empty())
}

/// Why the difference between the official server's layers `a` and Anchor's `b` at one block is
/// intended, if it is: Anchor sends a block of an intended name as it was stored, with no server
/// behaviour, and the official server may have changed it after loading the world (a bubble
/// column over no soul sand turns to water, a trial spawner wakes up); or Anchor sends `unknown`
/// (`info_update`) for a block whose name is intended, or for a waterlogged block the official
/// server sends as its water.
fn excuse(
    [a0, a1]: [u32; 2],
    [b0, b1]: [u32; 2],
    air: u32,
    unknown: u32,
    intended: &BTreeSet<String>,
    name: &impl Fn(u32) -> String,
) -> Option<String> {
    let water_for_second_layer = a1 == air && a0 == b1;
    if b0 != a0 && intended.contains(&name(b0)) && (a1 == b1 || water_for_second_layer) {
        return Some(name(b0));
    }
    if b0 == unknown && a0 != unknown {
        if intended.contains(&name(a0)) && a1 == b1 {
            return Some(name(a0));
        }
        if water_for_second_layer && name(b1).contains("water") {
            return Some("a waterlogged unknown block sent as its water".to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 8 payload with one layer of a single block, network ID 2.
    const STONE_PAYLOAD: &str = r#""CAEBBA==""#;

    /// A `SubChunk` packet line holding one entry at the centre.
    fn world_line(result: u8, payload: &str) -> String {
        format!(
            r#"{{"name":"SubChunk","fields":{{"Position":[0,0,0],"SubChunkEntries":[{{"Offset":[0,0,0],"Result":{result},"RawPayload":{payload}}}]}}}}"#
        )
    }

    fn states() -> BTreeMap<u32, String> {
        BTreeMap::from([
            (0, "minecraft:air[]".to_owned()),
            (1, "minecraft:info_update[]".to_owned()),
            (2, "minecraft:stone[]".to_owned()),
        ])
    }

    fn same(a: &str, b: &str) -> bool {
        let (a, b) = (
            sub_chunks(a).expect("the first capture holds sub chunks"),
            sub_chunks(b).expect("the second capture holds sub chunks"),
        );
        compare(
            &a,
            &b,
            &states(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            ["A", "B"],
            &mut Vec::new(),
        )
        .expect("the sub chunks can be compared")
    }

    #[test]
    fn an_all_air_answer_holds_no_layers() {
        let all_air = sub_chunks(&world_line(6, "null")).expect("an all air answer is read");
        assert_eq!(all_air.get(&[0, 0, 0]), Some(&Layers::new()));
    }

    #[test]
    fn an_all_air_answer_differs_from_a_sub_chunk_with_blocks() {
        let stone = world_line(1, STONE_PAYLOAD);
        let all_air = world_line(6, "null");
        assert!(!same(&stone, &all_air));
        assert!(same(&stone, &stone));
        assert!(same(&all_air, &all_air));
    }

    #[test]
    fn an_error_answer_is_not_compared() {
        let error = sub_chunks(&world_line(2, "null")).expect("an error answer is read");
        assert!(error.is_empty());
    }
}
