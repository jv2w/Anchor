//! `src/block/drops.rs`: the golden drops of every block state (PLAN.md 10.7) from the PMMP dump
//! (`blocks.json` `drops`), for the tests of the drop rules; the library does not read it.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::data::{BLOCK_ENTITY_DROPS, Data, State, StateItem, static_compound};
use super::emit::HEADER;

type R<T> = Result<T, String>;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/block/drops.rs";

/// Generates the file text (unformatted).
pub(crate) fn generate(data: &Data) -> R<String> {
    let mut stacks = String::new();
    let mut stack_count = 0;
    let mut stack_at: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut rows: Vec<String> = Vec::new();
    let mut row_at: BTreeMap<String, usize> = BTreeMap::new();
    let mut state_rows = Vec::with_capacity(data.states.len());
    let mut counts = [0usize; 3];
    for s in &data.states {
        let mut list = |items: Vec<String>| {
            let key = items.concat();
            *stack_at.entry(key).or_insert_with(|| {
                let at = (stack_count, items.len());
                stack_count += items.len();
                stacks.push_str(&items.concat());
                at
            })
        };
        let row = if s.drops == Json::String(BLOCK_ENTITY_DROPS.to_owned()) {
            counts[2] += 1;
            "Golden { hand: GoldenDrop::BLOCK_ENTITY, tool: None, silk_touch: None },".to_owned()
        } else {
            let hand = case(data, s, s.drops.get("hand"), &mut list, &mut counts)?
                .ok_or("drops without a hand case")?;
            let tool = case(data, s, s.drops.get("tool"), &mut list, &mut counts)?;
            let silk = case(data, s, s.drops.get("silk_touch"), &mut list, &mut counts)?
                .ok_or("drops without a silk touch case")?;
            format!(
                "Golden {{ hand: {}, tool: {}, silk_touch: Some({}) }},",
                hand.1,
                tool.map_or_else(
                    || "None".to_owned(),
                    |(t, d)| format!("Some((Type::from_index({}), {d}))", t.unwrap_or(0))
                ),
                format_args!("(Type::from_index({}), {})", silk.0.unwrap_or(0), silk.1)
            )
        };
        let index = *row_at.entry(row.clone()).or_insert_with(|| {
            rows.push(row);
            rows.len() - 1
        });
        state_rows.push(index);
    }
    let mut out = String::new();
    put!(
        out,
        "{HEADER}\n//! Golden drops generated from the PMMP dump (PLAN.md 10.7): {} deterministic \
         and {} random drop cases, {} states whose drops the block entity decides.\n\n\
         use super::{{Golden, GoldenDrop, GoldenItems, GoldenStack}};\nuse crate::item::Type;\n\n\
         /// The stacks of the deterministic drops, one list after another.\npub(super) static \
         STACKS: [GoldenStack; {stack_count}] = [{stacks}];\n\n/// The distinct golden rows.\n\
         pub(super) static GOLDEN: [Golden; {}] = [{}];\n\n/// The [`GOLDEN`] row of each \
         state.\npub(super) static STATE_GOLDEN: [u16; {}] = [",
        counts[0],
        counts[1],
        counts[2],
        rows.len(),
        rows.concat(),
        state_rows.len()
    );
    for r in &state_rows {
        put!(out, "{r},");
    }
    out.push_str("];\n");
    Ok(out)
}

/// One drop case (`{"tool": name, "items": ..., "xp": [min, max]}`) as Rust source, with the
/// tool's item type index; `None` if the case is absent. Counts deterministic and random cases.
fn case(
    data: &Data,
    state: &State,
    json: Option<&Json>,
    list: &mut impl FnMut(Vec<String>) -> (usize, usize),
    counts: &mut [usize; 3],
) -> R<Option<(Option<u16>, String)>> {
    let Some(json) = json else { return Ok(None) };
    let tool = match json.get("tool").and_then(Json::as_str) {
        Some(name) => Some(data.item_type(&serde_json::json!({ "name": name, "meta": 0 }))?),
        None => None,
    };
    let (min, max) = match json.get("xp").and_then(Json::as_array).map(Vec::as_slice) {
        None => (0, 0),
        Some([a, b]) => (
            a.as_u64().and_then(|v| u16::try_from(v).ok()).ok_or("xp")?,
            b.as_u64().and_then(|v| u16::try_from(v).ok()).ok_or("xp")?,
        ),
        Some(_) => return Err("xp is not [min, max]".to_owned()),
    };
    let items = match json.get("items").ok_or("drop case without items")? {
        Json::String(s) if s == "random" => {
            counts[1] += 1;
            "GoldenItems::Random".to_owned()
        }
        Json::String(s) if s == "world" => {
            counts[1] += 1;
            "GoldenItems::World".to_owned()
        }
        Json::String(s) if s == "self" => {
            counts[0] += 1;
            let StateItem::Item(item) = state.item else {
                return Err(format!("{} drops itself but has no item", state.key.name));
            };
            let it = data.items.get(usize::from(item)).ok_or("item index")?;
            let (start, len) = list(vec![stack(item, it.meta, 1, it.nbt.as_ref())]);
            format!("GoldenItems::Exact({start}, {len})")
        }
        Json::Array(stacks) => {
            counts[0] += 1;
            let mut out = Vec::with_capacity(stacks.len());
            for s in stacks {
                let st = data.stack(s)?;
                let count = u8::try_from(st.count).map_err(|_| "drop count")?;
                out.push(stack(st.item, st.meta, count, st.nbt.as_ref()));
            }
            let (start, len) = list(out);
            format!("GoldenItems::Exact({start}, {len})")
        }
        other => return Err(format!("unknown drop items {other}")),
    };
    Ok(Some((
        tool,
        format!("GoldenDrop {{ items: {items}, xp: ({min}, {max}) }}"),
    )))
}

/// Rust source of one golden stack.
fn stack(
    item: u16,
    meta: u16,
    count: u8,
    nbt: Option<&BTreeMap<String, super::data::Tag>>,
) -> String {
    format!(
        "GoldenStack {{ item: Type::from_index({item}), meta: {meta}, count: {count}, nbt: {} }},",
        nbt.map_or_else(
            || "None".to_owned(),
            |c| format!("Some({})", static_compound(c))
        )
    )
}
