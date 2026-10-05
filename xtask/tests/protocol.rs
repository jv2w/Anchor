//! Protocol conformance against gophertunnel (PLAN.md 4 P1–P4, 10.1 "M1 판정식"): corpus
//! coverage, decoding with no bytes left (P1), byte-identical round trips (P2), agreement with
//! gophertunnel in both directions (P3), and fixed-seed mutations that never panic plus
//! over-limit declarations that end in size errors (P4).

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

#[path = "support/oracle.rs"]
mod oracle;
#[path = "support/rng.rs"]
mod rng;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic;
use std::sync::OnceLock;

use anchor::nbt::{Compound, List, Value};
use anchor::protocol::batch::{self, Header, Settings};
use anchor::protocol::packet::{
    BlockActorData, BookEdit, ClientCacheBlobStatus, CommandRequest,
    ItemStackRequest as ItemStackRequestPacket, MobEquipment, Packet, PlayerEnchantOptions,
    RemoveObjective, ServerBoundPackSettingChange, SetActorData, SetTime, Text,
};
use anchor::protocol::{
    ConnectionRequest, EnchantmentOption, Error, ItemInstance, ItemStack, ItemStackRequest,
    ItemType, MetadataValue, PackSetting,
};
use oracle::Record;
use rng::Rng;

/// The shield's item network ID; the oracle uses the same value (`main.go` `ShieldID`).
const SHIELD_ID: i32 = 357;
/// The allocation budget of one decode: 2 × the default 16 MiB `max-batch-bytes`.
const BUDGET: usize = 2 * 16 * 1024 * 1024;
/// Packets in gophertunnel's client and server pools together.
const PACKET_COUNT: usize = 236;
/// Corpus samples per packet.
const SAMPLES: usize = 64;
/// Corpus seed.
const CORPUS_SEED: u64 = 1;
/// The per-field decode limits, which corpus samples keep within (Anchor rejects the rest).
const LIMITS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/reference/protocol_limits.json"
);
/// Mutated inputs per packet.
const MUTATIONS: usize = 1000;
/// Mutation seed; each packet mixes in its ID.
const MUTATION_SEED: u64 = 0x005e_ed0f_a4c4;

/// The corpus, generated once per test process and grouped by packet ID.
fn corpus() -> &'static BTreeMap<u32, Vec<Vec<u8>>> {
    static CORPUS: OnceLock<BTreeMap<u32, Vec<Vec<u8>>>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let seed = CORPUS_SEED.to_string();
        let samples = SAMPLES.to_string();
        let mut by_id: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
        for (id, data) in oracle::run(
            "corpus",
            &[
                "corpus", "-seed", &seed, "-samples", &samples, "-limits", LIMITS,
            ],
            None,
        ) {
            by_id.entry(id).or_default().push(data);
        }
        by_id
    })
}

/// The Rust variant name of a packet ID, for messages.
fn name(id: u32) -> String {
    Packet::from_id(id).map_or_else(
        || format!("unknown {id}"),
        |p| {
            let debug = format!("{p:?}");
            let variant = debug.split(['(', ' ']).next().unwrap_or_default();
            format!("{id} {variant}")
        },
    )
}

fn hex(bytes: &[u8]) -> String {
    const SHOWN: usize = 64;
    let mut s = String::new();
    for b in bytes.iter().take(SHOWN) {
        write!(s, "{b:02x}").expect("write to a String");
    }
    if bytes.len() > SHOWN {
        write!(s, "…(+{} bytes)", bytes.len() - SHOWN).expect("write to a String");
    }
    s
}

/// Where `a` and `b` first differ, with up to 24 bytes of each from 8 bytes before that point.
fn diff(a: &[u8], b: &[u8]) -> String {
    let at = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()));
    let from = at.saturating_sub(8);
    let window = |s: &[u8]| hex(&s[from.min(s.len())..(from + 24).min(s.len())]);
    format!(
        "first difference at byte {at} of {}/{}: [{}] vs [{}]",
        a.len(),
        b.len(),
        window(a),
        window(b)
    )
}

fn decode(id: u32, payload: &[u8]) -> anchor::protocol::Result<Packet> {
    let mut budget = BUDGET;
    Packet::decode(id, payload, &mut budget, SHIELD_ID)
}

fn encode(packet: &Packet) -> anchor::protocol::Result<Vec<u8>> {
    let mut out = Vec::new();
    packet.encode(&mut out, SHIELD_ID)?;
    Ok(out)
}

fn assert_no_failures(what: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{what}: {} failures\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_covers_every_packet_with_64_samples() {
    let known: BTreeSet<u32> = (0..=u32::from(u16::MAX))
        .filter(|&id| Packet::from_id(id).is_some())
        .collect();
    let corpus = corpus();
    let covered: BTreeSet<u32> = corpus.keys().copied().collect();
    assert_eq!(known.len(), PACKET_COUNT);
    assert_eq!(
        covered.symmetric_difference(&known).collect::<Vec<_>>(),
        Vec::<&u32>::new(),
        "corpus IDs and Rust packet IDs differ"
    );
    let short: Vec<String> = corpus
        .iter()
        .filter(|(_, samples)| samples.len() != SAMPLES)
        .map(|(&id, samples)| format!("{}: {} samples", name(id), samples.len()))
        .collect();
    assert_no_failures("sample count", &short);
}

/// P1: every sample decodes, with no bytes left over (`Packet::decode` rejects leftovers).
#[test]
fn every_corpus_sample_decodes_completely() {
    let mut failures = Vec::new();
    for (&id, samples) in corpus() {
        for (i, sample) in samples.iter().enumerate() {
            if let Err(e) = decode(id, sample) {
                failures.push(format!("{} sample {i}: {e} [{}]", name(id), hex(sample)));
            }
        }
    }
    assert_no_failures("P1", &failures);
}

/// P2 and P3(a): Rust re-encodes gophertunnel's bytes to the same bytes.
#[test]
fn every_corpus_sample_re_encodes_to_the_same_bytes() {
    let mut failures = Vec::new();
    for (&id, samples) in corpus() {
        for (i, sample) in samples.iter().enumerate() {
            let Ok(packet) = decode(id, sample) else {
                continue; // reported by the P1 test
            };
            match encode(&packet) {
                Ok(again) if again == *sample => {}
                Ok(again) => failures.push(format!(
                    "{} sample {i}: in vs out: {}",
                    name(id),
                    diff(sample, &again)
                )),
                Err(e) => failures.push(format!("{} sample {i}: encode: {e}", name(id))),
            }
        }
    }
    assert_no_failures("P2", &failures);
}

fn put_varuint(out: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        out.push((x & 0x7f).to_le_bytes()[0] | 0x80);
        x >>= 7;
    }
    out.push(x.to_le_bytes()[0]);
}

fn varuint(x: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varuint(&mut out, x);
    out
}

/// Replaces a varint-looking run (up to 5 bytes ending in a byte below 0x80) at a random
/// position with a large value, the way an attacker inflates a length or count.
fn inflate_varint(rng: &mut Rng, m: &mut Vec<u8>) {
    if m.is_empty() {
        return;
    }
    let start = rng.below(m.len());
    let Some(len) = m[start..].iter().take(5).position(|&b| b < 0x80) else {
        return;
    };
    let end = start + len + 1;
    let rest = u64::try_from(m.len() - end).expect("fits u64");
    let value = match rng.below(8) {
        0 => 1025,
        1 => 0x8000,
        2 => 0x1_0000,
        3 => 1 << 24,
        4 => 0x7fff_ffff,
        5 => 0xffff_ffff,
        6 => rest + 1,
        _ => rest * 2 + 2, // zigzag form of rest + 1 for signed lengths
    };
    m.splice(start..end, varuint(value));
}

/// One mutation of `sample`: 1–3 stacked bit flips, byte inserts or deletes, truncations and
/// varint inflations.
fn mutate(rng: &mut Rng, sample: &[u8]) -> Vec<u8> {
    let mut m = sample.to_vec();
    for _ in 0..=rng.below(3) {
        match rng.below(5) {
            0 if !m.is_empty() => {
                let at = rng.below(m.len());
                m[at] ^= 1 << rng.below(8);
            }
            1 => {
                let at = rng.below(m.len() + 1);
                let b = rng.byte();
                m.insert(at, b);
            }
            2 if !m.is_empty() => {
                m.remove(rng.below(m.len()));
            }
            3 => {
                let at = rng.below(m.len() + 1);
                m.truncate(at);
            }
            _ => inflate_varint(rng, &mut m),
        }
    }
    m
}

/// What decoding the mutations found, computed once and shared by the P4 and P3(b) tests.
#[derive(Default)]
struct MutationRun {
    inputs: usize,
    /// Inputs whose decode or re-encode panicked.
    panics: Vec<String>,
    /// Decoded inputs whose canonical encoding e does not satisfy `encode(decode(e)) == e`.
    unstable: Vec<String>,
    /// Rust encodings R of every decoded input.
    encodings: Vec<Record>,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic".to_owned())
}

/// Decodes `m`, then checks P2's second part on the result. Returns the canonical encoding e
/// when `m` decodes, and an idempotence failure message if one is found.
fn check_mutation(id: u32, m: &[u8]) -> (Option<Vec<u8>>, Option<String>) {
    let Ok(packet) = decode(id, m) else {
        return (None, None);
    };
    let e = match encode(&packet) {
        Ok(e) => e,
        Err(err) => return (None, Some(format!("encode(decode(m)): {err}"))),
    };
    let failure = match decode(id, &e).map(|p| encode(&p)) {
        Ok(Ok(again)) if again == e => None,
        Ok(Ok(again)) => Some(format!("e vs encode(decode(e)): {}", diff(&e, &again))),
        Ok(Err(err)) => Some(format!("re-encode: {err} e [{}]", hex(&e))),
        Err(err) => Some(format!("decode(e): {err} e [{}]", hex(&e))),
    };
    (Some(e), failure)
}

fn mutation_run() -> &'static MutationRun {
    static RUN: OnceLock<MutationRun> = OnceLock::new();
    RUN.get_or_init(|| {
        let mut run = MutationRun::default();
        for (&id, samples) in corpus() {
            let mut rng = Rng(MUTATION_SEED ^ u64::from(id));
            for i in 0..MUTATIONS {
                let m = mutate(&mut rng, &samples[i % samples.len()]);
                run.inputs += 1;
                let (e, failure) = match panic::catch_unwind(|| check_mutation(id, &m)) {
                    Ok(result) => result,
                    Err(payload) => {
                        let message = panic_message(payload.as_ref());
                        run.panics
                            .push(format!("{}: {message} m [{}]", name(id), hex(&m)));
                        continue;
                    }
                };
                if let Some(failure) = failure {
                    run.unstable
                        .push(format!("{}: {failure} m [{}]", name(id), hex(&m)));
                }
                if let Some(e) = e {
                    run.encodings.push((id, e));
                }
            }
        }
        run
    })
}

/// P4: no mutated input panics a decoder. P2 (second part): every decoded mutation's canonical
/// encoding is a fixed point.
#[test]
fn mutated_inputs_never_panic_and_decoded_ones_re_encode_stably() {
    let run = mutation_run();
    assert_eq!(run.inputs, PACKET_COUNT * MUTATIONS);
    assert_no_failures("P4 panics", &run.panics);
    assert_no_failures("P2 idempotence", &run.unstable);
    assert!(!run.encodings.is_empty(), "no mutation decoded");
}

/// Known, intended differences from gophertunnel's decode and re-encode, each reproduced
/// exactly (PLAN.md 10.1). Nothing else is excused.
///
/// - `CraftingData` material reducers: gophertunnel reads the input `mix` as `mix << 16` and
///   `mix & 0x7fff`, then writes `network_id << 16 | metadata`, so the input comes back as
///   `mix & 0x7fff`: network ID 0 and the low 15 bits of the metadata. Anchor reads
///   `mix >> 16` and `mix & 0xffff`.
/// - Empty NBT lists, in any packet: gophertunnel keeps the element type of an empty list only
///   for `TAG_Int` and `TAG_Long` and writes the others with `TAG_End`; Anchor keeps every
///   element type ([`anchor::nbt::List`]).
fn is_known_difference(id: u32, r: &[u8], returned: &[u8]) -> bool {
    let Ok(mut packet) = decode(id, r) else {
        return false;
    };
    if let Packet::CraftingData(p) = &mut packet {
        for reducer in &mut p.material_reducers {
            reducer.input_item.network_id = 0;
            reducer.input_item.metadata_value &= 0x7fff;
        }
    }
    let Ok(expected) = encode(&packet) else {
        return false;
    };
    differs_only_in_empty_list_types(id, &expected, returned)
}

/// NBT element types that gophertunnel drops from an empty list.
const DROPPED_EMPTY_LIST_TYPES: [(u8, &str); 10] = [
    (1, "Byte"),
    (2, "Short"),
    (5, "Float"),
    (6, "Double"),
    (7, "ByteArray"),
    (8, "String"),
    (9, "List"),
    (10, "Compound"),
    (11, "IntArray"),
    (12, "LongArray"),
];

/// Whether `returned` is `expected` with some empty NBT lists' element types replaced by
/// `TAG_End`: in the bytes, every difference is such a type byte, followed by a zero length,
/// turned into 0; in the decoded values, the packets are equal once those empty lists print
/// as `End` (packets hold NBT only in compounds, and no other type prints as `Short([])` etc.).
fn differs_only_in_empty_list_types(id: u32, expected: &[u8], returned: &[u8]) -> bool {
    let bytes_match = expected.len() == returned.len()
        && expected
            .iter()
            .zip(returned)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .all(|(at, (a, b))| {
                *b == 0
                    && DROPPED_EMPTY_LIST_TYPES.iter().any(|(tag, _)| tag == a)
                    && expected.get(at + 1) == Some(&0)
            });
    let (Ok(expected), Ok(returned)) = (decode(id, expected), decode(id, returned)) else {
        return false;
    };
    // An empty list prints as `X([])` after `List(` (a list value), `List([` or `, ` (an element
    // of a list of lists); an empty array value prints after `": ` and is kept.
    let mut erased = format!("{expected:?}");
    for (_, name) in DROPPED_EMPTY_LIST_TYPES {
        for before in ["List(", "List([", ", "] {
            erased = erased.replace(&format!("{before}{name}([])"), &format!("{before}End"));
        }
    }
    bytes_match && erased == format!("{returned:?}")
}

fn block_actor_data_with(position: [i32; 3], key: &str, value: Value) -> (u32, Vec<u8>) {
    let packet = Packet::BlockActorData(BlockActorData {
        position,
        nbt_data: [(key.to_owned(), value)].into_iter().collect(),
    });
    (packet.id(), encode(&packet).expect("encodes"))
}

#[test]
fn known_differences_excuse_only_what_gophertunnel_does() {
    let empty_short = Value::List(List::Short(Vec::new()));
    let empty_end = Value::List(List::End);
    let (id, r) = block_actor_data_with([1, 2, 3], "a", empty_short.clone());
    let (_, returned) = block_actor_data_with([1, 2, 3], "a", empty_end.clone());
    assert!(is_known_difference(id, &r, &returned));
    let nested = |inner| Value::List(List::List(vec![List::Int(vec![7]), inner]));
    let (_, r_nested) = block_actor_data_with([1, 2, 3], "a", nested(List::String(Vec::new())));
    let (_, returned_nested) = block_actor_data_with([1, 2, 3], "a", nested(List::End));
    assert!(is_known_difference(id, &r_nested, &returned_nested));
    // An empty int list keeps its type in gophertunnel too.
    let (_, r_int) = block_actor_data_with([1, 2, 3], "a", Value::List(List::Int(Vec::new())));
    assert!(!is_known_difference(id, &r_int, &returned));
    // An empty array value is not a list.
    let (_, r_array) = block_actor_data_with([1, 2, 3], "a", Value::IntArray(Vec::new()));
    let (_, returned_array) = block_actor_data_with([1, 2, 3], "a", Value::IntArray(Vec::new()));
    assert!(is_known_difference(id, &r_array, &returned_array));
    assert!(!is_known_difference(id, &r_array, &returned));
    // Any other change is not excused, even next to an excused one.
    let (_, moved) = block_actor_data_with([1, 0, 3], "a", empty_end);
    assert!(!is_known_difference(id, &r, &moved));
    let (_, renamed) = block_actor_data_with([1, 2, 3], "b", empty_short);
    assert!(!is_known_difference(id, &r, &renamed));
}

/// P3(b): gophertunnel decodes every Rust encoding R of a decoded mutation, and its
/// re-encoding G comes back through Rust as R.
#[test]
fn gophertunnel_accepts_rust_encodings_and_returns_the_same_packets() {
    let run = mutation_run();
    let results = oracle::run("p3b", &["check"], Some(run.encodings.as_slice()));
    assert_eq!(results.len(), run.encodings.len());
    let mut failures = Vec::new();
    for ((id, r), (status, g)) in run.encodings.iter().zip(&results) {
        let id = *id;
        if *status != 0 {
            failures.push(format!(
                "{}: gophertunnel rejects R: {} R [{}]",
                name(id),
                String::from_utf8_lossy(g),
                hex(r)
            ));
            continue;
        }
        match decode(id, g).map(|p| encode(&p)) {
            Ok(Ok(again)) if again == *r || is_known_difference(id, r, &again) => {}
            Ok(Ok(again)) => failures.push(format!(
                "{}: R vs Rust(G): {}; R vs G: {}",
                name(id),
                diff(r, &again),
                diff(r, g)
            )),
            Ok(Err(e)) => failures.push(format!("{}: re-encode of G: {e}", name(id))),
            Err(e) => failures.push(format!(
                "{}: Rust rejects G: {e} R [{}] G [{}]",
                name(id),
                hex(r),
                hex(g)
            )),
        }
    }
    assert_no_failures("P3(b)", &failures);
}

/// A compound with several keys at two levels.
fn multi_key_compound() -> Compound {
    let inner: Compound = [
        ("x".to_owned(), Value::Int(1)),
        ("y".to_owned(), Value::String("b".to_owned())),
        ("z".to_owned(), Value::Byte(3)),
    ]
    .into_iter()
    .collect();
    [
        ("alpha".to_owned(), Value::Short(7)),
        ("beta".to_owned(), Value::Compound(inner)),
        ("delta".to_owned(), Value::Float(0.5)),
        ("gamma".to_owned(), Value::Long(-9)),
    ]
    .into_iter()
    .collect()
}

/// P3(a) for NBT nested in item user data and entity metadata. The corpus gives those compounds
/// one key each, because gophertunnel's Writer writes the keys of a Go map in random order and a
/// sample needs fixed bytes; here gophertunnel decodes Rust's encoding R of compounds with several
/// keys, and its re-encoding G (keys in any order) decodes in Rust to the same packet, which
/// re-encodes to R.
#[test]
fn nested_compounds_with_several_keys_agree_with_gophertunnel() {
    let item = ItemInstance {
        stack_network_id: 5,
        stack: ItemStack {
            item_type: ItemType {
                network_id: 12,
                metadata_value: 0,
            },
            count: 1,
            nbt_data: multi_key_compound(),
            ..ItemStack::default()
        },
    };
    let packets = [
        Packet::MobEquipment(Box::new(MobEquipment {
            entity_runtime_id: 1,
            new_item: item,
            ..MobEquipment::default()
        })),
        Packet::SetActorData(SetActorData {
            entity_runtime_id: 1,
            entity_metadata: [(7, MetadataValue::CompoundTag(multi_key_compound()))]
                .into_iter()
                .collect(),
            ..SetActorData::default()
        }),
    ];
    let records: Vec<Record> = packets
        .iter()
        .map(|p| (p.id(), encode(p).expect("encodes")))
        .collect();
    let results = oracle::run("nested-nbt", &["check"], Some(records.as_slice()));
    assert_eq!(results.len(), records.len());
    for ((id, r), (status, g)) in records.iter().zip(&results) {
        assert_eq!(
            *status,
            0,
            "{}: gophertunnel rejects R: {}",
            name(*id),
            String::from_utf8_lossy(g)
        );
        let returned = decode(*id, g).expect("Rust decodes G");
        assert_eq!(returned, decode(*id, r).expect("Rust decodes R"));
        assert_eq!(encode(&returned).expect("re-encodes"), *r);
    }
}

fn assert_size_error(id: u32, payload: &[u8], budget: usize) {
    let mut left = budget;
    let result = Packet::decode(id, payload, &mut left, SHIELD_ID);
    assert!(
        matches!(result, Err(Error::Size)),
        "{}: expected a size error, got {result:?}",
        name(id)
    );
}

#[test]
fn a_string_longer_than_the_input_left_is_a_size_error() {
    let id = Packet::RemoveObjective(RemoveObjective::default()).id();
    let mut payload = varuint(6);
    payload.extend_from_slice(b"short");
    assert_size_error(id, &payload, BUDGET);
    for huge in [0x7fff_ffff, 0xffff_ffff] {
        let mut payload = varuint(huge);
        payload.extend_from_slice(b"short");
        assert_size_error(id, &payload, BUDGET);
    }
    let mut exact = varuint(5);
    exact.extend_from_slice(b"short");
    assert!(decode(id, &exact).is_ok());
}

/// `ClientCacheBlobStatus`: a varuint32 count of u64 miss hashes, then of hit hashes.
fn blob_status(misses: u64, data_hashes: usize) -> Vec<u8> {
    let mut payload = varuint(misses);
    payload.resize(payload.len() + 8 * data_hashes, 0xab);
    payload.push(0);
    payload
}

#[test]
fn a_list_count_over_1024_is_a_size_error() {
    let id = Packet::ClientCacheBlobStatus(ClientCacheBlobStatus::default()).id();
    assert_size_error(id, &blob_status(1025, 1025), BUDGET);
    assert_size_error(id, &blob_status(0xffff_ffff, 4), BUDGET);
    assert!(decode(id, &blob_status(1024, 1024)).is_ok());
}

#[test]
fn allocations_over_the_budget_are_size_errors() {
    let id = Packet::ClientCacheBlobStatus(ClientCacheBlobStatus::default()).id();
    let payload = blob_status(10, 10);
    assert_size_error(id, &payload, 79);
    let mut budget = 80;
    assert!(Packet::decode(id, &payload, &mut budget, SHIELD_ID).is_ok());
    assert_eq!(budget, 0, "the decode takes 10 × 8 bytes");
    assert_size_error(id, &payload, 0);
}

/// A translation `Text` payload with `params` parameters `"p"` and a filtered message of
/// `filtered` bytes, written by hand: the encoder refuses what gophertunnel refuses.
fn text_payload(params: usize, filtered: usize) -> Vec<u8> {
    let mut payload = vec![0, 2, 2, 1, b'm']; // not translated, category, type, message "m"
    put_varuint(&mut payload, u64::try_from(params).expect("fits u64"));
    payload.extend_from_slice(&[1, b'p'].repeat(params));
    payload.extend_from_slice(&[0, 0, 1]); // XUID, platform chat ID, filtered message present
    put_varuint(&mut payload, u64::try_from(filtered).expect("fits u64"));
    payload.resize(payload.len() + filtered, b'f');
    payload
}

/// `payload` with its string of `len` bytes `fill`, after its varuint length, one byte longer:
/// a payload over a limit the encoder refuses to write.
fn one_longer(payload: &[u8], fill: u8, len: usize) -> Vec<u8> {
    let mut run = varuint(u64::try_from(len).expect("fits u64"));
    run.resize(run.len() + len, fill);
    let at = payload
        .windows(run.len())
        .position(|w| w == run.as_slice())
        .expect("the string is in the payload");
    let (before, rest) = payload.split_at(at);
    let mut out = before.to_vec();
    put_varuint(&mut out, u64::try_from(len + 1).expect("fits u64"));
    out.resize(out.len() + len + 1, fill);
    out.extend_from_slice(rest.get(run.len()..).expect("the run is in the payload"));
    out
}

/// `payload`, a varuint count `count` (below 127) and that many equal elements, with one element
/// more: a payload over a limit the encoder refuses to write.
fn one_more(payload: &[u8], count: u8) -> Vec<u8> {
    let (first, elements) = payload.split_first().expect("a count");
    assert!(*first == count && count < 127, "a one-byte count {count}");
    let size = elements.len() / usize::from(count);
    assert_eq!(size * usize::from(count), elements.len(), "equal elements");
    let mut out = vec![count + 1];
    out.extend_from_slice(elements);
    out.extend_from_slice(elements.get(..size).expect("one element"));
    out
}

/// The translation `Text` of [`text_payload`].
fn text(params: usize, filtered: usize) -> Packet {
    Packet::Text(Box::new(Text {
        text_type: 2, // translation
        message: "m".to_owned(),
        parameters: vec!["p".to_owned(); params],
        filtered_message: Some("f".repeat(filtered)),
        ..Text::default()
    }))
}

/// Cases of one packet at a field's Mojang `maxLength` or `maxItems`, the same packet just over
/// it, and the payload of that packet.
fn field_limit_cases() -> Vec<(&'static str, Packet, Packet, Vec<u8>)> {
    let bytes = |packet: &Packet| encode(packet).expect("the packet at the limit encodes");
    let request = |filter: usize, requests: usize| {
        Packet::ItemStackRequest(ItemStackRequestPacket {
            requests: vec![
                ItemStackRequest {
                    filter_strings: vec!["f".repeat(filter)],
                    ..ItemStackRequest::default()
                };
                requests
            ],
        })
    };
    let pack_setting = |name: usize| {
        Packet::ServerBoundPackSettingChange(ServerBoundPackSettingChange {
            pack_setting: PackSetting {
                name: "n".repeat(name),
                ..PackSetting::default()
            },
            ..ServerBoundPackSettingChange::default()
        })
    };
    let command = |len: usize| {
        Packet::CommandRequest(CommandRequest {
            command_line: "c".repeat(len),
            ..CommandRequest::default()
        })
    };
    let book = |len: usize| {
        Packet::BookEdit(Box::new(BookEdit {
            action_type: 1, // add page
            text: "t".repeat(len),
            ..BookEdit::default()
        }))
    };
    let enchant = |count: usize| {
        Packet::PlayerEnchantOptions(PlayerEnchantOptions {
            options: vec![EnchantmentOption::default(); count],
        })
    };
    let at_text = text(4, 1);
    assert_eq!(
        bytes(&at_text),
        text_payload(4, 1),
        "the hand-written Text payload"
    );
    vec![
        (
            "CommandRequest.CommandLine (string)",
            command(1000),
            command(1001),
            one_longer(&bytes(&command(1000)), b'c', 1000),
        ),
        (
            "BookEdit.Text (string in a switch case)",
            book(768),
            book(769),
            one_longer(&bytes(&book(768)), b't', 768),
        ),
        (
            "PlayerEnchantOptions.Options (list)",
            enchant(3),
            enchant(4),
            one_more(&bytes(&enchant(3)), 3),
        ),
        (
            "ItemStackRequest.Requests (list)",
            request(0, 100),
            request(0, 101),
            one_more(&bytes(&request(0, 100)), 100),
        ),
        (
            "ItemStackRequest.FilterStrings[] (list element)",
            request(1000, 1),
            request(1001, 1),
            one_longer(&bytes(&request(1000, 1)), b'f', 1000),
        ),
        (
            "Text.Parameters (list)",
            at_text,
            text(5, 1),
            text_payload(5, 1),
        ),
        (
            "Text.FilteredMessage (optional string)",
            text(0, 65536),
            text(0, 65537),
            text_payload(0, 65537),
        ),
        (
            "PackSetting.Name (hand codec)",
            pack_setting(128),
            pack_setting(129),
            one_longer(&bytes(&pack_setting(128)), b'n', 128),
        ),
    ]
}

#[test]
fn a_field_over_its_mojang_limit_is_a_size_error() {
    for (what, at, over, payload) in field_limit_cases() {
        let encoded = encode(&at).expect("the packet at the limit encodes");
        assert_eq!(decode(at.id(), &encoded).as_ref(), Ok(&at), "{what}");
        assert_eq!(encode(&over), Err(Error::Size), "{what}: the encoder refuses it");
        assert_eq!(decode(at.id(), &payload).err(), Some(Error::Size), "{what}");
    }
}

/// `BlockActorData` at the origin holding a network NBT root `nbt`.
fn block_actor_data(nbt: &[u8]) -> (u32, Vec<u8>) {
    let id = Packet::BlockActorData(BlockActorData::default()).id();
    let mut payload = vec![0, 0, 0];
    payload.extend_from_slice(nbt);
    (id, payload)
}

/// A root compound with `depth - 1` compounds nested inside, all with empty names.
fn nested_compounds(depth: usize) -> Vec<u8> {
    let mut nbt = Vec::with_capacity(4 * depth);
    for _ in 0..depth {
        nbt.extend_from_slice(&[10, 0]);
    }
    nbt.resize(nbt.len() + depth, 0);
    nbt
}

#[test]
fn nbt_nested_deeper_than_512_is_a_size_error() {
    let (id, payload) = block_actor_data(&nested_compounds(513));
    assert_size_error(id, &payload, BUDGET);
    let (id, payload) = block_actor_data(&nested_compounds(512));
    assert!(decode(id, &payload).is_ok());
}

/// A root compound holding one byte array of `len` bytes.
fn byte_array_root(len: usize) -> Vec<u8> {
    let mut nbt = vec![10, 0, 7, 0];
    let len = u64::try_from(len).expect("fits u64");
    put_varuint(&mut nbt, len * 2); // varint32 zigzag
    nbt.resize(nbt.len() + usize::try_from(len).expect("fits usize"), 0x5a);
    nbt.push(0);
    nbt
}

#[test]
fn an_nbt_root_over_4_mib_is_a_size_error() {
    const MIB: usize = 1024 * 1024;
    let (id, payload) = block_actor_data(&byte_array_root(4 * MIB));
    assert_size_error(id, &payload, BUDGET);
    let (id, payload) = block_actor_data(&byte_array_root(4 * MIB - 16));
    assert!(decode(id, &payload).is_ok());
}

/// A batch payload of `count` `SetTime` packets with time 0: length 2, header, varint 0.
fn set_time_batch(count: usize) -> Vec<u8> {
    let id = Packet::SetTime(SetTime::default()).id();
    let header = u8::try_from(id).expect("one-byte header");
    [2, header, 0].repeat(count)
}

#[test]
fn a_batch_of_more_than_812_packets_is_a_size_error() {
    let size = batch::Error::Packet(Error::Size);
    let mut out = Vec::new();
    let settings = Settings::default();
    assert_eq!(batch::split(&set_time_batch(812)).map(|p| p.len()), Ok(812));
    batch::decode_packets(&set_time_batch(812), &settings, SHIELD_ID, &mut out)
        .expect("812 packets decode");
    assert_eq!(out.len(), 812);
    assert_eq!(
        batch::split(&set_time_batch(813)).map(|p| p.len()),
        Err(size)
    );
    assert_eq!(
        batch::decode_packets(&set_time_batch(813), &settings, SHIELD_ID, &mut out),
        Err(size)
    );
    assert!(out.is_empty());
}

/// One sample of every packet in one batch payload, through the public batch encoder.
fn corpus_batch() -> Vec<u8> {
    let packets: Vec<(Header, Packet)> = corpus()
        .iter()
        .map(|(&id, samples)| {
            let header = Header {
                id,
                ..Header::default()
            };
            (header, decode(id, &samples[0]).expect("P1"))
        })
        .collect();
    let mut payloads = Vec::new();
    let mut scratch = Vec::new();
    batch::encode_packets(
        packets.iter().map(|(h, p)| (*h, p)),
        SHIELD_ID,
        &mut scratch,
        |payload| {
            payloads.push(payload.to_vec());
            Ok(())
        },
    )
    .expect("the corpus encodes");
    assert_eq!(payloads.len(), 1, "the corpus fits one payload");
    payloads.remove(0)
}

/// P4 for the decoders outside `Packet::decode`: batch payload splitting and decoding, and the
/// Login connection request envelope.
#[test]
fn mutated_batch_payloads_and_connection_requests_never_panic() {
    let settings = Settings::default();
    let payload = corpus_batch();
    let mut out = Vec::new();
    batch::decode_packets(&payload, &settings, SHIELD_ID, &mut out).expect("the batch decodes");
    assert_eq!(out.len(), PACKET_COUNT);

    let mut request = Vec::new();
    for part in [&b"{\"chain\":[]}"[..], b"header.claims.signature"] {
        let len = i32::try_from(part.len()).expect("short");
        request.extend_from_slice(&len.to_le_bytes());
        request.extend_from_slice(part);
    }
    ConnectionRequest::parse(&request).expect("the request parses");

    let mut rng = Rng(MUTATION_SEED);
    let mut panics = Vec::new();
    for _ in 0..MUTATIONS {
        let m = mutate(&mut rng, &payload);
        if let Err(e) = panic::catch_unwind(|| {
            let mut out = Vec::new();
            batch::decode_packets(&m, &settings, SHIELD_ID, &mut out).map(|()| out.len())
        }) {
            panics.push(format!(
                "batch: {} m [{}]",
                panic_message(e.as_ref()),
                hex(&m)
            ));
        }
        let m = mutate(&mut rng, &request);
        if let Err(e) = panic::catch_unwind(|| ConnectionRequest::parse(&m)) {
            panics.push(format!(
                "request: {} m [{}]",
                panic_message(e.as_ref()),
                hex(&m)
            ));
        }
    }
    assert_no_failures("P4 panics", &panics);
}

#[test]
fn connection_request_lengths_over_the_input_are_size_errors() {
    let mut chain_over = 100_i32.to_le_bytes().to_vec();
    chain_over.extend_from_slice(b"{}");
    let mut token_over = 2_i32.to_le_bytes().to_vec();
    token_over.extend_from_slice(b"{}");
    token_over.extend_from_slice(&i32::MAX.to_le_bytes());
    let mut negative = (-1_i32).to_le_bytes().to_vec();
    negative.extend_from_slice(b"{}");
    for request in [chain_over, token_over, negative] {
        assert_eq!(ConnectionRequest::parse(&request), Err(Error::Size));
    }
}

/// Packets whose default value gophertunnel's writer refuses too: Text needs a message, and
/// `ClientMovementPredictionSync`'s actor flags need a bitset of their fixed size.
const INVALID_DEFAULTS: &[u32] = &[9, 322];

#[test]
fn every_packet_id_has_a_default_value() {
    let ids: Vec<u32> = (0..=u16::MAX.into())
        .filter(|&id| Packet::from_id(id).is_some())
        .collect();
    assert_eq!(ids.len(), 236);
    for id in ids {
        let packet = Packet::from_id(id).expect("listed");
        assert_eq!(packet.id(), id);
        assert!(Packet::is_from_client(id) || Packet::is_from_server(id));
    }
}

#[test]
fn default_packets_round_trip() {
    let mut failures = Vec::new();
    for id in 0..=u32::from(u16::MAX) {
        let Some(packet) = Packet::from_id(id) else {
            continue;
        };
        let mut bytes = vec![0xaa];
        match packet.encode(&mut bytes, SHIELD_ID) {
            Ok(()) => {}
            Err(e) => {
                assert_eq!(
                    bytes,
                    [0xaa],
                    "{id}: a failed encode leaves the buffer as it was"
                );
                if !INVALID_DEFAULTS.contains(&id) {
                    failures.push(format!("{id} {packet:?}: encode: {e}"));
                }
                continue;
            }
        }
        let payload = bytes.get(1..).expect("the marker byte");
        let mut budget = BUDGET;
        let decoded = match Packet::decode(id, payload, &mut budget, SHIELD_ID) {
            Ok(decoded) => decoded,
            Err(e) => {
                failures.push(format!("{id}: decode: {e}"));
                continue;
            }
        };
        assert!(budget <= BUDGET);
        let mut again = Vec::new();
        decoded
            .encode(&mut again, SHIELD_ID)
            .expect("a decoded packet encodes");
        if again != payload {
            failures.push(format!("{id}: re-encoded bytes differ"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn trailing_bytes_and_unknown_ids_are_errors() {
    let packet = Packet::from_id(1).expect("Login");
    let mut bytes = Vec::new();
    packet.encode(&mut bytes, SHIELD_ID).expect("encodes");
    bytes.push(0);
    let mut budget = BUDGET;
    assert!(Packet::decode(1, &bytes, &mut budget, SHIELD_ID).is_err());
    assert!(Packet::decode(0, &[], &mut budget, SHIELD_ID).is_err());
}
