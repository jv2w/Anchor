//! The trace of a game scenario capture: the packets after the scenario starts, grouped by the
//! bot's steps, each turned into a line. A line is *fixed* (compared between servers) or *shown*
//! (in the report only). What is fixed (PLAN.md 10.7 P6: response status, changed slots, block
//! states, window types, offer shape):
//!
//! - the bot's commands and request statuses, and `ItemStackResponse` slots (container, slot,
//!   count, durability, name; stack network IDs as `#n`, numbered in order of appearance);
//! - the items the client sees at the end of each step (the bot's `state` record: inventory, UI
//!   and open container slots with item name and count). How a server delivers them
//!   (`InventorySlot`, `InventoryContent`, server `InventoryTransaction`, how often it resends) is
//!   shown only: BDS resends every window twice after a block placement;
//! - `ContainerOpen` and `ContainerClose` (window IDs other than the fixed inventory windows as
//!   `w#n`, numbered in order of appearance), the keys of `ContainerSetData` per window over the
//!   whole capture (a last step, `ContainerSetData`: when a value arrives is timing);
//! - the last state of each position a step's `UpdateBlock` and `UpdateSubChunkBlocks` packets set
//!   (layer 0; PLAN.md 10.6 block changes) when it differs from the state the client knew before
//!   the step (an earlier update, else the flat world's), the keys of `BlockActorData`,
//!   `LevelEvent` types. BDS resends the blocks around a clicked block after every placement (P6
//!   facts 1); a resend that changes nothing is shown only;
//! - entities by type (`AddActor`, `AddItemActor` item, `RemoveActor`, `TakeItemActor` taker);
//! - `UpdateTrade` window type, size, tier and flags, the key and tag types of the offers;
//! - the number of `PlayerEnchantOptions` and the player's last experience level of a step when
//!   it differs from the level before the step (BDS resends every attribute after a placement).
//!
//! Shown only, because they vary per run or by design: random values (enchantment options,
//! offers, a villager's profession and name, experience orbs and points), timing
//! (`ContainerSetData` values and counts, entity motion), sounds, chat, recipe unlocks,
//! `SetActorData`, positions of dropped items, and `Event` types: the only one BDS sends in the
//! scenarios is `EntityInteract` when trading opens, which feeds the client's statistics and
//! changes nothing the client does, so Anchor does not send it (P6 facts 8). Within a step the
//! fixed lines are compared as a sorted list, so packets of one step that a server sends in
//! another order still match.
//!
//! The bot marks two kinds of steps. In a *setup* step (console commands that prepare the world)
//! only the commands are fixed: how a server answers a command is not gameplay. A run of *random*
//! steps (bone meal until the crop is grown, picking up a random number of drops, trading with a
//! villager of a random profession) becomes one step whose fixed lines are the distinct *shapes*
//! of its lines: item names, block states, counts and stack IDs replaced by `?`, commands shown
//! only.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::super::compare::field;

/// The flat world's blocks by height (BDS's and Anchor's default flat world), as the bot assumes
/// them (`blockAt` in `xtask/go/bot/game.go`); air elsewhere.
const FLAT: [(i64, &str); 4] = [
    (-64, "minecraft:bedrock[infiniburn_bit=0]"),
    (-63, "minecraft:dirt[]"),
    (-62, "minecraft:dirt[]"),
    (-61, "minecraft:grass_block[]"),
];
/// A new player's experience level.
const START_LEVEL: &str = "0";

/// The windows whose IDs mean the same on every server: the inventory, off hand, armour and UI.
const FIXED_WINDOWS: [i64; 4] = [0, 119, 120, 124];

/// Checks whose values are fixed between servers.
const FIXED_CHECKS: [&str; 6] = [
    "auto craft variant",
    "deprecated result form",
    "place a crafting table",
    "place a crafting table held with a wrong block runtime ID",
    "legacy villager",
    "legacy trading",
];

/// Entities whose appearance is random: experience orbs (their number and values).
const RANDOM_ENTITIES: [&str; 1] = ["minecraft:xp_orb"];
/// Level events that follow random entities: an experience orb picked up.
const RANDOM_LEVEL_EVENTS: [i64; 1] = [1051];

/// Packets whose number per step is shown and nothing else.
const COUNTED: [&str; 18] = [
    "LevelChunk",
    "SubChunk",
    "NetworkChunkPublisherUpdate",
    "MoveActorDelta",
    "MoveActorAbsolute",
    "SetActorMotion",
    "MovePlayer",
    "SetActorData",
    "MobEquipment",
    "MobArmourEquipment",
    "MobEffect",
    "Animate",
    "Text",
    "UnlockedRecipes",
    "SyncWorldClocks",
    "CurrentStructureFeature",
    "PlayerHotBar",
    "LevelSoundEvent",
];

/// One step of a trace.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Step {
    /// The bot's name for the step.
    pub(crate) name: String,
    /// Lines compared between servers.
    pub(crate) fixed: Vec<String>,
    /// Lines for the report only.
    pub(crate) shown: Vec<String>,
    /// The bot marked the step as world setup.
    pub(crate) setup: bool,
    /// The bot marked the step as random.
    pub(crate) random: bool,
}

/// Turns a capture into its trace. `items` maps item network IDs to names, `blocks` block
/// network IDs to states.
pub(crate) fn trace(
    capture: &str,
    items: &BTreeMap<i64, String>,
    blocks: &BTreeMap<u32, String>,
) -> Result<Vec<Step>, String> {
    let mut t = Tracer {
        items,
        blocks,
        steps: Vec::new(),
        current: None,
        player: None,
        stacks: BTreeMap::new(),
        windows: BTreeMap::new(),
        runtime: BTreeMap::new(),
        unique: BTreeMap::new(),
        data: BTreeMap::new(),
        keys: BTreeMap::new(),
        placed: BTreeMap::new(),
        known: BTreeMap::new(),
        level: None,
        known_level: START_LEVEL.to_owned(),
        state: None,
        counted: BTreeMap::new(),
    };
    for (i, line) in capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        t.line(&value);
    }
    t.end_step();
    let mut steps = collapse(t.steps);
    if !t.keys.is_empty() {
        steps.push(Step {
            name: "ContainerSetData".to_owned(),
            fixed: t
                .keys
                .iter()
                .map(|(window, keys)| format!("ContainerSetData window {window} keys {keys:?}"))
                .collect(),
            ..Step::default()
        });
    }
    Ok(steps)
}

/// The flat world's block at a position written as `[x,y,z]`.
fn flat(position: &str) -> &'static str {
    let y = position
        .trim_matches(['[', ']'])
        .split(',')
        .nth(1)
        .and_then(|y| y.trim().parse::<i64>().ok());
    FLAT.iter()
        .find(|(height, _)| Some(*height) == y)
        .map_or("minecraft:air[]", |(_, state)| state)
}

/// Merges each run of random steps into its first step, with the distinct shapes of their fixed
/// lines.
fn collapse(steps: Vec<Step>) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::with_capacity(steps.len());
    for mut step in steps {
        if step.random {
            let fixed = std::mem::take(&mut step.fixed);
            let (commands, others): (Vec<String>, Vec<String>) =
                fixed.into_iter().partition(|l| l.starts_with("command "));
            step.shown.splice(0..0, commands);
            step.fixed = others.iter().map(|l| shape(l)).collect();
            if let Some(last) = out.last_mut().filter(|s| s.random) {
                last.fixed.append(&mut step.fixed);
                last.shown.push(format!("(step {})", step.name));
                last.shown.append(&mut step.shown);
                last.fixed.sort();
                last.fixed.dedup();
                continue;
            }
            step.fixed.sort();
            step.fixed.dedup();
        }
        out.push(step);
    }
    out
}

/// The shape of a line: item names and block states (`minecraft:...`), counts (`x12`), stack IDs
/// (`#3`) and the slots of response slots (`12:0`) replaced by `?`; item states and inventory
/// packets by their first word alone (which slot a random item lands in is random too).
fn shape(line: &str) -> String {
    if line.starts_with("Inventory") || line.starts_with("items ") {
        return line.split(' ').next().unwrap_or_default().to_owned();
    }
    line.split(' ')
        .map(|token| {
            if token.starts_with("minecraft:") {
                return "minecraft:?".to_owned();
            }
            if let Some((container, slot)) = token.trim_start_matches('[').split_once(':')
                && !container.is_empty()
                && container.bytes().all(|b| b.is_ascii_digit())
                && !slot.is_empty()
                && slot.bytes().all(|b| b.is_ascii_digit())
            {
                let open = if token.starts_with('[') { "[" } else { "" };
                return format!("{open}{container}:?");
            }
            for prefix in ['x', '#'] {
                if let Some(rest) = token.strip_prefix(prefix) {
                    let digits =
                        rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                    if digits > 0 {
                        let tail = rest.get(digits..).unwrap_or_default();
                        return format!("{prefix}?{tail}");
                    }
                }
            }
            token.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The item names of a full item capture's `ItemRegistry`.
pub(crate) fn item_names(items_capture: &str) -> Result<BTreeMap<i64, String>, String> {
    for (i, line) in items_capture.lines().enumerate() {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if field(&value, "name").as_str() != Some("ItemRegistry") {
            continue;
        }
        let entries = field(field(&value, "fields"), "Items")
            .as_array()
            .ok_or("ItemRegistry without Items")?;
        return entries
            .iter()
            .map(|e| {
                let id = field(e, "RuntimeID")
                    .as_i64()
                    .ok_or("item without RuntimeID")?;
                let name = field(e, "Name").as_str().ok_or("item without Name")?;
                Ok((id, name.to_owned()))
            })
            .collect();
    }
    Err("no ItemRegistry".to_owned())
}

/// The state of a trace in the making.
#[derive(Debug)]
struct Tracer<'a> {
    items: &'a BTreeMap<i64, String>,
    blocks: &'a BTreeMap<u32, String>,
    steps: Vec<Step>,
    current: Option<Step>,
    /// The player's runtime ID (from `StartGame`).
    player: Option<i64>,
    /// Stack network IDs and window IDs in order of appearance.
    stacks: BTreeMap<i64, usize>,
    windows: BTreeMap<i64, usize>,
    /// Entities by runtime ID and unique ID: what they are.
    runtime: BTreeMap<i64, String>,
    unique: BTreeMap<i64, String>,
    /// `ContainerSetData` of the current step: (window, key) -> (count, last value).
    data: BTreeMap<(String, i64), (usize, i64)>,
    /// `ContainerSetData` keys of the whole capture by window.
    keys: BTreeMap<String, BTreeSet<i64>>,
    /// The last block state per position, the last player level and the bot's last item state of
    /// the current step.
    placed: BTreeMap<String, String>,
    /// The block state per position as the client knows it at the start of the current step, for
    /// positions updated before it; others hold the flat world's block ([`flat`]).
    known: BTreeMap<String, String>,
    level: Option<String>,
    /// The player's experience level at the start of the current step.
    known_level: String,
    state: Option<String>,
    /// Counted packets of the current step.
    counted: BTreeMap<String, usize>,
}

impl Tracer<'_> {
    fn line(&mut self, v: &Value) {
        match field(v, "type").as_str() {
            Some("step") => {
                self.end_step();
                self.current = Some(Step {
                    name: field(v, "name").as_str().unwrap_or_default().to_owned(),
                    setup: field(v, "setup").as_bool() == Some(true),
                    random: field(v, "random").as_bool() == Some(true),
                    ..Step::default()
                });
            }
            Some("command") => self.fixed(format!("command {}", text(field(v, "line")))),
            Some("state") => {
                let slots: Vec<String> = field(v, "slots")
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(slot, item)| format!("{slot} {}", text(item)))
                    .collect();
                self.state = Some(format!("items [{}]", slots.join(", ")));
            }
            Some("response") => self.fixed(format!("response status {}", field(v, "status"))),
            Some("check") => {
                let name = field(v, "name").as_str().unwrap_or_default();
                let line = format!("check {name}: {}", compact(field(v, "value")));
                if FIXED_CHECKS.contains(&name) {
                    self.fixed(line);
                } else if name == "offers" || name == "offers on reopening" {
                    self.fixed(format!(
                        "check {name}: shape {}",
                        offer_shape(field(v, "value"))
                    ));
                    self.shown(line);
                } else {
                    self.shown(line);
                }
            }
            Some("packet") => self.packet(v),
            _ => {}
        }
    }

    fn fixed(&mut self, line: String) {
        if let Some(step) = &mut self.current {
            step.fixed.push(line);
        }
    }

    fn shown(&mut self, line: String) {
        if let Some(step) = &mut self.current {
            step.shown.push(line);
        }
    }

    fn end_step(&mut self) {
        let Some(mut step) = self.current.take() else {
            return;
        };
        for (position, state) in std::mem::take(&mut self.placed) {
            let line = format!("block {position} {state}");
            let before = self
                .known
                .get(&position)
                .map_or_else(|| flat(&position), String::as_str);
            if before == state {
                step.shown.push(format!("{line} (unchanged)"));
            } else {
                step.fixed.push(line);
            }
            self.known.insert(position, state);
        }
        if let Some(level) = self.level.take() {
            let line = format!("player level {level}");
            if level == self.known_level {
                step.shown.push(format!("{line} (unchanged)"));
            } else {
                step.fixed.push(line);
            }
            self.known_level = level;
        }
        if let Some(state) = self.state.take() {
            step.fixed.push(state);
        }
        for ((window, key), (count, last)) in std::mem::take(&mut self.data) {
            step.shown.push(format!(
                "ContainerSetData window {window} key {key}: {count} packets, last {last}"
            ));
        }
        for (name, count) in std::mem::take(&mut self.counted) {
            step.shown.push(format!("{count} x {name}"));
        }
        if step.setup {
            let fixed = std::mem::take(&mut step.fixed);
            let (commands, others): (Vec<String>, Vec<String>) =
                fixed.into_iter().partition(|l| l.starts_with("command "));
            step.fixed = commands;
            step.shown.splice(0..0, others);
        }
        step.fixed.sort();
        self.steps.push(step);
    }

    /// A stack network ID as `#n`, numbered in order of appearance; 0 (empty) and negative IDs
    /// (request IDs) as they are.
    fn stack(&mut self, id: &Value) -> String {
        let id = id.as_i64().unwrap_or_default();
        if id <= 0 {
            return id.to_string();
        }
        let next = self.stacks.len() + 1;
        format!("#{}", self.stacks.entry(id).or_insert(next))
    }

    /// A window ID: the fixed inventory windows as they are, others as `w#n`.
    fn window(&mut self, id: &Value) -> String {
        let id = id.as_i64().unwrap_or_default();
        if FIXED_WINDOWS.contains(&id) {
            return id.to_string();
        }
        let next = self.windows.len() + 1;
        format!("w#{}", self.windows.entry(id).or_insert(next))
    }

    /// An item stack (`ItemStack` JSON) as text: name, count, meta, block state and NBT.
    fn item(&self, stack: &Value) -> String {
        let id = field(field(stack, "ItemType"), "NetworkID")
            .as_i64()
            .unwrap_or_default();
        let count = field(stack, "Count").as_i64().unwrap_or_default();
        if id == 0 || count == 0 {
            return "empty".to_owned();
        }
        let name = self
            .items
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("item {id}"));
        let mut parts = vec![format!("{name} x{count}")];
        let meta = field(field(stack, "ItemType"), "MetadataValue");
        if meta.as_i64().unwrap_or_default() != 0 {
            parts.push(format!("meta {meta}"));
        }
        let block = field(stack, "BlockRuntimeID").as_i64().unwrap_or_default();
        if block != 0 {
            parts.push(format!("block {}", self.block(block)));
        }
        let nbt = field(field(stack, "NBTData"), "compound");
        if nbt.as_object().is_some_and(|m| !m.is_empty()) {
            parts.push(format!("nbt {}", compact(nbt)));
        }
        parts.join(" ")
    }

    /// A block network ID as its state, or the number when it is unknown.
    fn block(&self, id: i64) -> String {
        // Network IDs are u32 hashes; captures hold them as i32 or u32.
        let key = u32::try_from(id)
            .or_else(|_| i32::try_from(id).map(i32::cast_unsigned))
            .ok();
        key.and_then(|k| self.blocks.get(&k))
            .cloned()
            .unwrap_or_else(|| format!("block {id}"))
    }

    /// An item instance (`ItemInstance` JSON): the item and its stack network ID.
    fn instance(&mut self, instance: &Value) -> String {
        let item = self.item(field(instance, "Stack"));
        let id = self.stack(field(instance, "StackNetworkID"));
        format!("{item} {id}")
    }

    fn packet(&mut self, v: &Value) {
        let name = field(v, "name").as_str().unwrap_or_default().to_owned();
        let f = field(v, "fields");
        if name == "StartGame" {
            self.player = field(f, "EntityRuntimeID").as_i64();
        }
        if self.current.is_none() {
            return;
        }
        if v.get("error").is_some() {
            self.fixed(format!("{name} failed to decode"));
            return;
        }
        if COUNTED.contains(&name.as_str()) {
            if name == "LevelSoundEvent" {
                self.shown(format!("LevelSoundEvent {}", compact(f)));
            }
            *self.counted.entry(name).or_default() += 1;
            return;
        }
        match name.as_str() {
            "ItemStackResponse" => self.responses(f),
            "InventorySlot" | "InventoryContent" | "InventoryTransaction" => {
                self.inventory(&name, f);
            }
            "ContainerOpen" | "ContainerClose" | "ContainerSetData" => self.container(&name, f),
            "UpdateBlock" | "UpdateSubChunkBlocks" | "BlockActorData" | "LevelEvent" => {
                self.world(&name, f);
            }
            "AddActor" | "AddItemActor" | "RemoveActor" | "TakeItemActor" => self.entity(&name, f),
            "UpdateTrade" | "PlayerEnchantOptions" => self.screen(&name, f),
            "UpdateAttributes" if field(f, "EntityRuntimeID").as_i64() == self.player => {
                self.attributes(f);
            }
            "Event" => self.shown(format!(
                "Event {}",
                field(v, "event").as_str().unwrap_or("unknown")
            )),
            "Disconnect" => self.fixed(format!("Disconnect {}", compact(f))),
            _ => *self.counted.entry(name).or_default() += 1,
        }
    }

    /// Inventory packets: shown only (the bot's `state` record holds what they result in).
    fn inventory(&mut self, name: &str, f: &Value) {
        let line = match name {
            "InventorySlot" => {
                let window = self.window(field(f, "WindowID"));
                let item = self.instance(field(f, "NewItem"));
                format!(
                    "InventorySlot window {window} slot {} {item}",
                    field(f, "Slot")
                )
            }
            "InventoryContent" => {
                let window = self.window(field(f, "WindowID"));
                let mut slots = Vec::new();
                for (slot, instance) in field(f, "Content")
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let item = self.instance(instance);
                    if !item.starts_with("empty") {
                        slots.push(format!("{slot}: {item}"));
                    }
                }
                format!("InventoryContent window {window} [{}]", slots.join(", "))
            }
            _ => {
                let mut actions = Vec::new();
                for a in field(f, "Actions").as_array().into_iter().flatten() {
                    let old = self.instance(field(a, "OldItem"));
                    let new = self.instance(field(a, "NewItem"));
                    actions.push(format!(
                        "source {} window {} slot {}: {old} -> {new}",
                        field(a, "SourceType"),
                        field(a, "WindowID"),
                        field(a, "InventorySlot")
                    ));
                }
                format!("InventoryTransaction [{}]", actions.join("; "))
            }
        };
        self.shown(line);
    }

    /// Container windows: opening and closing are fixed, data keys are collected.
    fn container(&mut self, name: &str, f: &Value) {
        let window = self.window(field(f, "WindowID"));
        match name {
            "ContainerOpen" => self.fixed(format!(
                "ContainerOpen window {window} type {} position {} entity {}",
                field(f, "ContainerType"),
                field(f, "ContainerPosition"),
                field(f, "ContainerEntityUniqueID")
            )),
            "ContainerClose" => self.fixed(format!(
                "ContainerClose window {window} type {} server side {}",
                field(f, "ContainerType"),
                field(f, "ServerSide")
            )),
            _ => {
                let key = field(f, "Key").as_i64().unwrap_or_default();
                let value = field(f, "Value").as_i64().unwrap_or_default();
                self.keys.entry(window.clone()).or_default().insert(key);
                let entry = self.data.entry((window, key)).or_insert((0, 0));
                entry.0 += 1;
                entry.1 = value;
            }
        }
    }

    /// Block changes (their last state per position), block entity data and level events.
    fn world(&mut self, name: &str, f: &Value) {
        match name {
            "UpdateBlock" => {
                let state = self.block(field(f, "NewBlockRuntimeID").as_i64().unwrap_or_default());
                let line = format!(
                    "UpdateBlock {} {state} flags {} layer {}",
                    field(f, "Position"),
                    field(f, "Flags"),
                    field(f, "Layer")
                );
                if field(f, "Layer").as_i64() == Some(0) {
                    self.placed.insert(field(f, "Position").to_string(), state);
                }
                self.shown(line);
            }
            "UpdateSubChunkBlocks" => {
                for key in ["Blocks", "Extra"] {
                    for e in field(f, key).as_array().into_iter().flatten() {
                        let state =
                            self.block(field(e, "BlockRuntimeID").as_i64().unwrap_or_default());
                        let line = format!(
                            "UpdateSubChunkBlocks {key} {} {state} flags {}",
                            field(e, "BlockPos"),
                            field(e, "Flags")
                        );
                        if key == "Blocks" {
                            self.placed.insert(field(e, "BlockPos").to_string(), state);
                        }
                        self.shown(line);
                    }
                }
            }
            "BlockActorData" => {
                let nbt = field(field(f, "NBTData"), "compound");
                let keys: Vec<&String> = nbt
                    .as_object()
                    .map(|m| m.keys().collect())
                    .unwrap_or_default();
                self.fixed(format!(
                    "BlockActorData {} keys {keys:?}",
                    field(f, "Position")
                ));
                self.shown(format!(
                    "BlockActorData {} {}",
                    field(f, "Position"),
                    compact(nbt)
                ));
            }
            _ => {
                let event = field(f, "EventType");
                if !RANDOM_LEVEL_EVENTS.contains(&event.as_i64().unwrap_or_default()) {
                    self.fixed(format!("LevelEvent {event}"));
                }
                self.shown(format!("LevelEvent {}", compact(f)));
            }
        }
    }

    /// Entities, by what they are.
    fn entity(&mut self, name: &str, f: &Value) {
        let kind_of = |map: &BTreeMap<i64, String>, key: &str| {
            map.get(&field(f, key).as_i64().unwrap_or_default())
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned())
        };
        match name {
            "AddActor" | "RemoveActor" => {
                let kind = if name == "AddActor" {
                    let kind = text(field(f, "EntityType"));
                    self.remember(f, &kind);
                    kind
                } else {
                    kind_of(&self.unique, "EntityUniqueID")
                };
                let line = format!("{name} {kind}");
                if RANDOM_ENTITIES.contains(&kind.as_str()) {
                    self.shown(line);
                } else {
                    self.fixed(line);
                }
            }
            "AddItemActor" => {
                let item = self.item(field(field(f, "Item"), "Stack"));
                self.remember(f, &format!("item {item}"));
                self.fixed(format!("AddItemActor {item}"));
                self.shown(format!("AddItemActor {item} at {}", field(f, "Position")));
            }
            _ => {
                let kind = kind_of(&self.runtime, "ItemEntityRuntimeID");
                let taker = if field(f, "TakerEntityRuntimeID").as_i64() == self.player {
                    "the player"
                } else {
                    "another entity"
                };
                self.fixed(format!("TakeItemActor {kind} by {taker}"));
            }
        }
    }

    /// Trading and enchanting screens.
    fn screen(&mut self, name: &str, f: &Value) {
        if name == "UpdateTrade" {
            let window = self.window(field(f, "WindowID"));
            self.fixed(format!(
                "UpdateTrade window {window} type {} size {} tier {} new UI {} demand based prices {}",
                field(f, "WindowType"),
                field(f, "Size"),
                field(f, "TradeTier"),
                field(f, "NewTradeUI"),
                field(f, "DemandBasedPrices")
            ));
            self.shown(format!(
                "UpdateTrade display name {} villager {} entity {}",
                field(f, "DisplayName"),
                field(f, "VillagerUniqueID"),
                field(f, "EntityUniqueID")
            ));
            return;
        }
        let options = field(f, "Options").as_array().map_or(0, Vec::len);
        self.fixed(format!("PlayerEnchantOptions {options} options"));
        if options > 0 {
            self.shown(format!("PlayerEnchantOptions {}", compact(f)));
        }
    }

    /// The player's attributes: the level is fixed (commands and enchanting change it by fixed
    /// amounts), experience points and the exhaustion of actions vary.
    fn attributes(&mut self, f: &Value) {
        let mut values = Vec::new();
        for a in field(f, "Attributes").as_array().into_iter().flatten() {
            let value = field(a, "AttributeValue");
            let name = text(field(value, "Name"));
            if name == "minecraft:player.level" {
                self.level = Some(field(value, "Value").to_string());
            }
            values.push(format!("{name}={}", field(value, "Value")));
        }
        self.shown(format!("UpdateAttributes player {}", values.join(" ")));
    }

    /// Remembers what an added entity is, by its runtime and unique IDs.
    fn remember(&mut self, f: &Value, kind: &str) {
        if let Some(id) = field(f, "EntityRuntimeID").as_i64() {
            self.runtime.insert(id, kind.to_owned());
        }
        if let Some(id) = field(f, "EntityUniqueID").as_i64() {
            self.unique.insert(id, kind.to_owned());
        }
    }

    fn responses(&mut self, f: &Value) {
        for r in field(f, "Responses").as_array().into_iter().flatten() {
            let mut slots = Vec::new();
            for info in field(r, "ContainerInfo").as_array().into_iter().flatten() {
                let container = field(field(info, "Container"), "ContainerID");
                for s in field(info, "SlotInfo").as_array().into_iter().flatten() {
                    let id = self.stack(field(s, "StackNetworkID"));
                    slots.push(format!(
                        "{container}:{} x{} {id} durability {} name {}",
                        field(s, "Slot"),
                        field(s, "Count"),
                        field(s, "DurabilityCorrection"),
                        field(s, "CustomName")
                    ));
                }
            }
            self.fixed(format!(
                "ItemStackResponse request {} status {} [{}]",
                field(r, "RequestID"),
                field(r, "Status"),
                slots.join(", ")
            ));
        }
    }
}

/// A JSON string without quotes, other values as JSON.
fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// A value as compact JSON.
fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// The shape of an offer list (typed NBT JSON): its top-level keys, and the keys and tag types
/// that every recipe has (also inside its item compounds), without values. Keys only some offers
/// have (`buyB`, an item's `Block` or `tag`) depend on the offers, which are random.
fn offer_shape(offers: &Value) -> String {
    let root = field(offers, "compound");
    let top: Vec<&String> = root
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    let mut common: Option<BTreeSet<String>> = None;
    for recipe in field(field(root, "Recipes"), "list")
        .as_array()
        .into_iter()
        .flatten()
    {
        let mut keys = BTreeSet::new();
        for (k, typed) in field(recipe, "compound").as_object().into_iter().flatten() {
            keys.insert(format!("{k}:{}", tag(typed)));
            for (inner, inner_typed) in field(typed, "compound").as_object().into_iter().flatten() {
                keys.insert(format!("{k}.{inner}:{}", tag(inner_typed)));
            }
        }
        common = Some(match common {
            Some(c) => c.intersection(&keys).cloned().collect(),
            None => keys,
        });
    }
    format!(
        "top {top:?} keys of every recipe {:?}",
        common.unwrap_or_default()
    )
}

/// The tag type of a typed NBT JSON value (`{"int":1}` is `int`).
fn tag(typed: &Value) -> String {
    typed
        .as_object()
        .and_then(|t| t.keys().next())
        .cloned()
        .unwrap_or_default()
}

/// Compares two traces' fixed lines step by step and returns the differences.
pub(crate) fn compare(a: &[Step], b: &[Step]) -> Vec<String> {
    let mut differences = Vec::new();
    let names = |s: &[Step]| s.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
    if names(a) != names(b) {
        differences.push(format!(
            "the steps differ:\n  {:?}\n  {:?}",
            names(a),
            names(b)
        ));
        return differences;
    }
    for (x, y) in a.iter().zip(b) {
        if x.fixed == y.fixed {
            continue;
        }
        let only = |p: &[String], q: &[String]| {
            let mut q: Vec<&String> = q.iter().collect();
            p.iter()
                .filter(|line| {
                    q.iter()
                        .position(|o| o == line)
                        .map(|i| q.remove(i))
                        .is_none()
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        differences.push(format!(
            "step {}: only in the first: {:?}; only in the second: {:?}",
            x.name,
            only(&x.fixed, &y.fixed),
            only(&y.fixed, &x.fixed)
        ));
    }
    differences
}

/// The report text of a trace.
pub(crate) fn report(steps: &[Step]) -> String {
    let mut lines = Vec::new();
    for step in steps {
        lines.push(format!("step {}", step.name));
        lines.extend(step.fixed.iter().map(|line| format!("  = {line}")));
        lines.extend(step.shown.iter().map(|line| format!("  . {line}")));
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> BTreeMap<i64, String> {
        BTreeMap::from([(17, "minecraft:oak_log".to_owned())])
    }

    fn blocks() -> BTreeMap<u32, String> {
        BTreeMap::from([
            (1, "minecraft:stone[]".to_owned()),
            (2, "minecraft:air[]".to_owned()),
        ])
    }

    const CAPTURE: &str = concat!(
        r#"{"type":"packet","name":"StartGame","fields":{"EntityRuntimeID":6}}"#,
        "\n",
        r#"{"type":"step","name":"give"}"#,
        "\n",
        r#"{"type":"command","line":"give P6Craft oak_log 4"}"#,
        "\n",
        r#"{"type":"packet","name":"InventorySlot","fields":{"WindowID":0,"Slot":0,"NewItem":{"StackNetworkID":57,"Stack":{"ItemType":{"NetworkID":17,"MetadataValue":0},"Count":4,"BlockRuntimeID":0,"NBTData":{"compound":{}}}}}}"#,
        "\n",
        r#"{"type":"packet","name":"Text","fields":{}}"#,
        "\n",
        r#"{"type":"state","slots":{"0:0":"minecraft:oak_log x4"}}"#,
        "\n",
        r#"{"type":"step","name":"open"}"#,
        "\n",
        r#"{"type":"packet","name":"ContainerOpen","fields":{"WindowID":9,"ContainerType":2,"ContainerPosition":[1,2,3],"ContainerEntityUniqueID":-1}}"#,
        "\n",
        r#"{"type":"packet","name":"ContainerSetData","fields":{"WindowID":9,"Key":1,"Value":5}}"#,
        "\n",
        r#"{"type":"packet","name":"ContainerSetData","fields":{"WindowID":9,"Key":1,"Value":4}}"#,
        "\n",
        r#"{"type":"packet","name":"ItemStackResponse","fields":{"Responses":[{"RequestID":-1,"Status":0,"ContainerInfo":[{"Container":{"ContainerID":12},"SlotInfo":[{"Slot":0,"Count":3,"StackNetworkID":57,"DurabilityCorrection":0,"CustomName":""}]}]}]}}"#,
        "\n",
        r#"{"type":"packet","name":"UpdateBlock","fields":{"Position":[1,2,3],"NewBlockRuntimeID":2,"Flags":3,"Layer":0}}"#,
        "\n",
        r#"{"type":"packet","name":"UpdateBlock","fields":{"Position":[1,2,3],"NewBlockRuntimeID":1,"Flags":3,"Layer":0}}"#,
        "\n",
        r#"{"type":"packet","name":"TakeItemActor","fields":{"ItemEntityRuntimeID":3,"TakerEntityRuntimeID":6}}"#,
        "\n",
    );

    #[test]
    fn traces_keep_the_item_state_and_the_last_block_states_of_each_step() {
        let steps = trace(CAPTURE, &items(), &blocks()).expect("trace");
        let give = steps.first().expect("give");
        assert_eq!(give.name, "give");
        assert_eq!(
            give.fixed,
            [
                "command give P6Craft oak_log 4",
                "items [0:0 minecraft:oak_log x4]",
            ]
        );
        assert_eq!(
            give.shown,
            [
                "InventorySlot window 0 slot 0 minecraft:oak_log x4 #1",
                "1 x Text"
            ]
        );
        let open = steps.get(1).expect("open");
        assert_eq!(
            open.fixed,
            [
                "ContainerOpen window w#1 type 2 position [1,2,3] entity -1",
                r#"ItemStackResponse request -1 status 0 [12:0 x3 #1 durability 0 name ""]"#,
                "TakeItemActor unknown by the player",
                "block [1,2,3] minecraft:stone[]",
            ]
        );
        assert!(
            open.shown
                .contains(&"ContainerSetData window w#1 key 1: 2 packets, last 4".to_owned())
        );
        let data = steps.last().expect("keys");
        assert_eq!(data.fixed, ["ContainerSetData window w#1 keys {1}"]);
    }

    #[test]
    fn resent_blocks_and_levels_that_change_nothing_are_shown_only() {
        let update = |pos: &str, id: u32| {
            format!(
                r#"{{"type":"packet","name":"UpdateBlock","fields":{{"Position":{pos},"NewBlockRuntimeID":{id},"Flags":3,"Layer":0}}}}"#
            )
        };
        let level = |value: u32| {
            format!(
                r#"{{"type":"packet","name":"UpdateAttributes","fields":{{"EntityRuntimeID":6,"Attributes":[{{"AttributeValue":{{"Name":"minecraft:player.level","Value":{value}}}}}]}}}}"#
            )
        };
        let capture = [
            r#"{"type":"packet","name":"StartGame","fields":{"EntityRuntimeID":6}}"#.to_owned(),
            r#"{"type":"step","name":"place"}"#.to_owned(),
            // A placed stone, the air above it resent, the level resent.
            update("[1,-60,1]", 1),
            update("[1,-59,1]", 2),
            level(0),
            r#"{"type":"step","name":"break"}"#.to_owned(),
            // The stone broken, then the air resent: a change from the stone.
            update("[1,-60,1]", 2),
            update("[1,-60,1]", 2),
            level(30),
            r#"{"type":"step","name":"again"}"#.to_owned(),
            update("[1,-60,1]", 2),
            level(30),
        ]
        .join("\n");
        let steps = trace(&capture, &items(), &blocks()).expect("trace");
        let fixed: Vec<&[String]> = steps.iter().map(|s| s.fixed.as_slice()).collect();
        assert_eq!(
            fixed,
            [
                &["block [1,-60,1] minecraft:stone[]".to_owned()][..],
                &[
                    "block [1,-60,1] minecraft:air[]".to_owned(),
                    "player level 30".to_owned()
                ][..],
                &[][..],
            ]
        );
        assert!(
            steps[0]
                .shown
                .contains(&"block [1,-59,1] minecraft:air[] (unchanged)".to_owned())
        );
        assert!(
            steps[0]
                .shown
                .contains(&"player level 0 (unchanged)".to_owned())
        );
    }

    #[test]
    fn event_packets_are_shown_only() {
        let capture = CAPTURE.replace(
            r#"{"type":"packet","name":"Text","fields":{}}"#,
            r#"{"type":"packet","name":"Event","event":"EntityInteract","fields":{}}"#,
        );
        let with = trace(&capture, &items(), &blocks()).expect("trace");
        let without = trace(CAPTURE, &items(), &blocks()).expect("trace");
        let give = with.first().expect("give");
        assert!(give.shown.contains(&"Event EntityInteract".to_owned()));
        let fixed = |steps: &[Step]| steps.iter().map(|s| s.fixed.clone()).collect::<Vec<_>>();
        assert_eq!(fixed(&with), fixed(&without));
    }

    #[test]
    fn the_flat_world_is_known_below_the_first_air_layer() {
        assert_eq!(flat("[3,-61,-4]"), "minecraft:grass_block[]");
        assert_eq!(flat("[3,-62,-4]"), "minecraft:dirt[]");
        assert_eq!(flat("[3,-64,-4]"), "minecraft:bedrock[infiniburn_bit=0]");
        assert_eq!(flat("[3,-60,-4]"), "minecraft:air[]");
        assert_eq!(flat("not a position"), "minecraft:air[]");
    }

    #[test]
    fn traces_of_runs_with_other_ids_compare_equal() {
        let other = CAPTURE
            .replace("57", "91")
            .replace(r#""WindowID":9"#, r#""WindowID":3"#);
        let a = trace(CAPTURE, &items(), &blocks()).expect("a");
        let b = trace(&other, &items(), &blocks()).expect("b");
        assert!(compare(&a, &b).is_empty());
        let changed = CAPTURE.replace(r#""Count":3"#, r#""Count":2"#);
        let c = trace(&changed, &items(), &blocks()).expect("c");
        let differences = compare(&a, &c);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert!(differences.first().is_some_and(|d| d.contains("x2")));
    }

    #[test]
    fn setup_steps_compare_their_commands_only() {
        let capture = CAPTURE.replace(r#""name":"give"}"#, r#""name":"give","setup":true}"#);
        let steps = trace(&capture, &items(), &blocks()).expect("trace");
        let give = steps.first().expect("give");
        assert_eq!(give.fixed, ["command give P6Craft oak_log 4"]);
        assert!(
            give.shown
                .contains(&"items [0:0 minecraft:oak_log x4]".to_owned())
        );
    }

    #[test]
    fn runs_of_random_steps_become_one_step_of_shapes() {
        let step = |n: i64| {
            format!(
                concat!(
                    r#"{{"type":"step","name":"bone meal","random":true}}"#,
                    "\n",
                    r#"{{"type":"packet","name":"ItemStackResponse","fields":{{"Responses":[{{"RequestID":-1,"Status":0,"ContainerInfo":[{{"Container":{{"ContainerID":12}},"SlotInfo":[{{"Slot":0,"Count":{},"StackNetworkID":{},"DurabilityCorrection":0,"CustomName":""}}]}}]}}]}}}}"#,
                    "\n"
                ),
                20 - n,
                10 + n
            )
        };
        let short: String = (0..2).map(step).collect();
        let long: String = (0..5).map(step).collect();
        let a = trace(&short, &items(), &blocks()).expect("a");
        let b = trace(&long, &items(), &blocks()).expect("b");
        assert_eq!(a.len(), 1);
        assert_eq!(
            a.first().expect("step").fixed,
            [r#"ItemStackResponse request -1 status 0 [12:? x? #? durability 0 name ""]"#]
        );
        assert!(compare(&a, &b).is_empty());
    }

    #[test]
    fn shapes_hide_names_counts_and_stack_ids() {
        assert_eq!(
            shape("ItemStackResponse request -1 status 0 [12:0 x62 #1 durability 0, 47:4 x2 #8]"),
            "ItemStackResponse request -1 status 0 [12:? x? #? durability 0, 47:? x? #?]"
        );
        assert_eq!(
            shape("block [1,2,3] minecraft:wheat[growth=3]"),
            "block [1,2,3] minecraft:?"
        );
        assert_eq!(shape("InventorySlot window 0 slot 2 x"), "InventorySlot");
        assert_eq!(shape("items [0:0 minecraft:wheat x1]"), "items");
    }

    #[test]
    fn offer_shapes_keep_the_keys_every_recipe_has() {
        let offers: Value = serde_json::from_str(
            r#"{"compound":{"Recipes":{"list":[{"compound":{"buyA":{"compound":{"Count":{"byte":20}}},"netId":{"int":4772},"buyB":{"compound":{}}}},{"compound":{"buyA":{"compound":{"Count":{"byte":1}}},"netId":{"int":4773}}}]},"TierExpRequirements":{"list":[]}}}"#,
        )
        .expect("json");
        assert_eq!(
            offer_shape(&offers),
            r#"top ["Recipes", "TierExpRequirements"] keys of every recipe {"buyA.Count:byte", "buyA:compound", "netId:int"}"#
        );
    }
}
