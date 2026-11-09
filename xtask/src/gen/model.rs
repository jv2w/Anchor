//! What the generator knows besides the IR: the hand-written Rust types and IO methods, the
//! Go type to Rust type mapping, and lookups over the IR.

use std::collections::{BTreeMap, BTreeSet};

use super::ir::{Enum, Expr, Field, Function, Ir, Stmt, Type, TypeDef, Union};
use super::names;

/// Union variants and packets larger than this many bytes (estimated) are boxed, so the enums
/// stay small to move and `clippy::large_enum_variant` holds.
const BOX_OVER: u64 = 128;

/// External Go types: Go name, Rust type, `Copy`.
const EXTERNAL_TYPES: &[(&str, &str, bool)] = &[
    ("mgl32.Vec3", "[f32; 3]", true),
    ("mgl32.Vec2", "[f32; 2]", true),
    ("uuid.UUID", "Uuid", true),
    ("color.RGBA", "Rgba", true),
];

/// Hand types that implement `Codec`, so a `marshal` statement may name them.
const HAND_CODECS: &[&str] = &[
    "protocol.ItemInstance",
    "protocol.StackRequestItem",
    "protocol.MaterialReducer",
    "protocol.MaterialReducerOutput",
    "protocol.GameRule",
    "protocol.PackSetting",
];

/// Functions written by hand as `Reader`/`Writer` methods, with the method name: the first
/// argument is the place read or written, the others are `Copy` values the method takes after
/// it. A Go union value is a pointer, so a body that copies one into a local and changes it
/// through the copy has no value-type translation (`src/protocol/union_frame.rs`); the input
/// flags are checked for duplicates with a bit mask instead of the quadratic search the Go body
/// has (`src/protocol/flags.rs`).
const HAND_FUNCTIONS: &[(&str, &str)] = &[
    ("protocol.InputFlagList", "input_flag_list"),
    (
        "protocol.StackRequestItemDescriptorCount",
        "stack_request_item_descriptor_count",
    ),
];

/// Go types with a hand-written Rust type (`src/protocol/*.rs`): Go name, Rust name, whether the
/// Rust type is `Copy`, and its size in bytes, which decides boxing; the generated code asserts
/// each size at compile time.
const HAND_TYPES: &[(&str, &str, bool, u64)] = &[
    ("protocol.BlockPos", "BlockPos", true, 12),
    ("protocol.ChunkPos", "ChunkPos", true, 8),
    ("protocol.SubChunkPos", "SubChunkPos", true, 12),
    ("protocol.Bitset", "Bitset", false, 32),
    ("protocol.EntityMetadata", "EntityMetadata", false, 24),
    ("protocol.ItemType", "ItemType", true, 8),
    ("protocol.ItemStack", "ItemStack", false, 96),
    ("protocol.ItemInstance", "ItemInstance", false, 104),
    ("protocol.StackRequestItem", "StackRequestItem", false, 120),
    ("protocol.MaterialReducer", "MaterialReducer", false, 32),
    (
        "protocol.MaterialReducerOutput",
        "MaterialReducerOutput",
        true,
        8,
    ),
    ("protocol.GameRule", "GameRule", false, 40),
    ("protocol.PackSetting", "PackSetting", false, 56),
];

/// The `protocol.IO` methods and their Rust form (the `Reader` and `Writer` methods in
/// `src/protocol/` and `src/nbt.rs` name them; the writer signatures decide `pass` and
/// `write_fails`).
const IO_METHODS: &[(&str, IoSpec)] = &[
    ("AbilityValue", io("ability_value", Pass::Value)),
    ("ActorRuntimeID", io("actor_runtime_id", Pass::Value)),
    (
        "ActorRuntimeIDVarint64",
        io("actor_runtime_id_varint64", Pass::Value),
    ),
    (
        "ActorRuntimeIDVaruint32",
        io("actor_runtime_id_varuint32", Pass::Value),
    ),
    ("ActorUniqueID", io("actor_unique_id", Pass::Value)),
    (
        "ActorUniqueIDInt64",
        io("actor_unique_id_int64", Pass::Value),
    ),
    (
        "ActorUniqueIDUint64",
        io("actor_unique_id_uint64", Pass::Value),
    ),
    (
        "ActorUniqueIDVaruint64",
        io("actor_unique_id_varuint64", Pass::Value),
    ),
    ("BEARGB", io("be_argb", Pass::Value)),
    ("BEInt32", io("be_int32", Pass::Value)),
    ("Bitset", io_fallible("bitset", Pass::Ref)),
    ("BlockPos", io("block_pos", Pass::Value)),
    ("Bool", io("bool", Pass::Value)),
    ("ByteFloat", io("byte_float", Pass::Value)),
    ("ByteSlice", io_fallible("byte_slice", Pass::Ref)),
    ("Bytes", io("bytes", Pass::Ref)),
    ("ChunkPos", io("chunk_pos", Pass::Value)),
    ("EntityMetadata", io_fallible("entity_metadata", Pass::Ref)),
    (
        "EventOrdinal",
        IoSpec {
            rust: "event_ordinal",
            pass: Pass::Ref,
            write_fails: true,
            read: Read::Checks,
        },
    ),
    ("EventType", io("event_type", Pass::Ref)),
    ("Float32", io("float32", Pass::Value)),
    ("Float64", io("float64", Pass::Value)),
    ("GameRule", io_fallible("game_rule", Pass::Ref)),
    ("Int16", io("int16", Pass::Value)),
    ("Int32", io("int32", Pass::Value)),
    ("Int64", io("int64", Pass::Value)),
    ("Int8", io("int8", Pass::Value)),
    ("Item", io_fallible("item", Pass::Ref)),
    (
        "ItemDescriptorCount",
        io_fallible("item_descriptor_count", Pass::Ref),
    ),
    ("ItemInstance", io_fallible("item_instance", Pass::Ref)),
    (
        "MaterialReducer",
        io_fallible("material_reducer", Pass::Ref),
    ),
    ("NBT", io_fallible("nbt", Pass::Ref)),
    ("PackSetting", io_fallible("pack_setting", Pass::Ref)),
    (
        "PlayerInventoryAction",
        io_fallible("player_inventory_action", Pass::Ref),
    ),
    ("RGB", io("rgb", Pass::Value)),
    ("RGBA", io("rgba", Pass::Value)),
    ("ShapeData", io_fallible("shape_data", Pass::Ref)),
    ("SoundPos", io("sound_pos", Pass::Value)),
    (
        "StackRequestAction",
        io_fallible("stack_request_action", Pass::Ref),
    ),
    (
        "StackRequestItem",
        io_fallible("stack_request_item", Pass::Ref),
    ),
    ("String", io_fallible("string", Pass::Ref)),
    ("StringUTF", io_fallible("string_utf", Pass::Ref)),
    ("SubChunkPos", io("sub_chunk_pos", Pass::Value)),
    (
        "TransactionDataType",
        io("transaction_data_type", Pass::Ref),
    ),
    ("UUID", io("uuid", Pass::Value)),
    ("Uint16", io("uint16", Pass::Value)),
    ("Uint32", io("uint32", Pass::Value)),
    ("Uint64", io("uint64", Pass::Value)),
    ("Uint8", io("uint8", Pass::Value)),
    ("Varint32", io("varint32", Pass::Value)),
    ("Varint64", io("varint64", Pass::Value)),
    ("Varuint32", io("varuint32", Pass::Value)),
    ("Varuint64", io("varuint64", Pass::Value)),
    ("Vec2", io("vec2", Pass::Value)),
    ("Vec3", io("vec3", Pass::Value)),
];

/// IO methods that are not statements: they appear as expressions and `error` statements.
const IO_NON_STATEMENTS: &[&str] = &["ShieldID", "InvalidValue", "UnknownEnumOption"];

/// IO methods that no `Marshal` calls, so they have no Rust form; a call fails generation.
const IO_UNUSED: &[&str] = &["NBTList"];

/// Struct fields (`<Go type>.<field>`) that gophertunnel reads and writes as raw `Bytes` but that
/// hold one network little-endian NBT root: generated as `nbt::Compound`, so the tick hands the
/// compound over and the packet's encoding, off the tick, writes it.
const NBT_FIELDS: &[&str] = &["packet.UpdateTrade.SerialisedOffers"];

/// The IR argument of the network little-endian NBT encoding.
const NBT_NETWORK_ARG: &str = r#"{"kind": "external", "name": "nbt.NetworkLittleEndian", "type": {"kind": "external", "name": "nbt.networkLittleEndian"}}"#;

/// The IR type of an NBT compound (`map[string]any`).
const NBT_TYPE: &str =
    r#"{"kind": "map", "key": {"kind": "basic", "name": "string"}, "elem": {"kind": "any"}}"#;

/// Struct fields (`<Go type>.<field>`) held as `Arc<T>`: the server sends one value of them
/// to many players (a skin of up to 4 MiB in the player list and skin broadcasts, a villager's
/// offers of up to 2 MiB to every trader), so every packet shares it instead of copying it.
const SHARED_FIELDS: &[&str] = &[
    "protocol.PlayerListEntry.Skin",
    "packet.PlayerSkin.Skin",
    "packet.UpdateTrade.SerialisedOffers",
];

/// How a writer takes an IO method's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    /// By value (`Copy` values).
    Value,
    /// By reference.
    Ref,
}

/// How a reader produces an IO method's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Read {
    /// `r.method(args)?` returns the value.
    Returns,
    /// `r.method(&place)?` checks the current value and returns nothing.
    Checks,
}

/// One `protocol.IO` method in Rust.
#[derive(Debug, Clone, Copy)]
pub(super) struct IoSpec {
    /// The Rust method name on `Reader` and `Writer`.
    pub(super) rust: &'static str,
    /// How the writer takes the value.
    pub(super) pass: Pass,
    /// Whether the writer returns `Result`.
    pub(super) write_fails: bool,
    /// How the reader produces the value.
    pub(super) read: Read,
}

/// The Rust module a generated item lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Module {
    /// `src/protocol/types.rs`.
    Types,
    /// `src/protocol/packet.rs`.
    Packet,
}

/// A union interface: its variants and lookup functions.
#[derive(Debug)]
pub(super) struct UnionInfo {
    /// The Rust enum name.
    pub(super) rust: String,
    /// Variants in ID table order: Go type and Rust variant name.
    pub(super) variants: Vec<(String, String)>,
    /// The `id`-key lookup (ID to variant).
    pub(super) by_id: String,
    /// The `type`-key lookups (variant to ID).
    pub(super) by_type: Vec<String>,
}

/// Everything the emitter and translator look up.
#[derive(Debug)]
pub(super) struct Model<'a> {
    pub(super) ir: &'a Ir,
    pub(super) types: BTreeMap<&'a str, &'a TypeDef>,
    pub(super) functions: BTreeMap<&'a str, &'a Function>,
    pub(super) enums: BTreeMap<&'a str, &'a Enum>,
    pub(super) unions: BTreeMap<&'a str, &'a Union>,
    /// Union interfaces by Go name.
    pub(super) interfaces: BTreeMap<String, UnionInfo>,
    /// Rust names of generated types by Go name.
    pub(super) rust_names: BTreeMap<String, (String, Module)>,
    /// Struct fields of type `any` written with `AbilityValue`.
    pub(super) ability_fields: BTreeSet<(String, String)>,
    /// Packet type names.
    pub(super) packets: BTreeSet<&'a str>,
    /// Decode limits by field (`<Go type>.<field>`, `[]` for its list elements): the most bytes
    /// of a string or elements of a list (`xtask/data/reference/protocol_limits.json`).
    pub(super) limits: BTreeMap<String, usize>,
}

impl<'a> Model<'a> {
    pub(super) fn new(ir: &'a Ir, limits: BTreeMap<String, usize>) -> Result<Self, String> {
        let names: Vec<&str> = ir.io_methods.iter().map(|m| m.name.as_str()).collect();
        for name in &names {
            if !IO_NON_STATEMENTS.contains(name)
                && !IO_UNUSED.contains(name)
                && io_spec(name).is_none()
            {
                return Err(format!("IO method {name} has no Rust form in IO_METHODS"));
            }
        }
        let listed = IO_METHODS
            .iter()
            .map(|(name, _)| name)
            .chain(IO_NON_STATEMENTS)
            .chain(IO_UNUSED);
        for name in listed {
            if !names.contains(name) {
                return Err(format!(
                    "{name} is listed as an IO method, which the IR does not have"
                ));
            }
        }
        let mut model = Self {
            ir,
            types: ir.types.iter().map(|t| (t.name.as_str(), t)).collect(),
            functions: ir.functions.iter().map(|f| (f.name.as_str(), f)).collect(),
            enums: ir.enums.iter().map(|e| (e.name.as_str(), e)).collect(),
            unions: ir.unions.iter().map(|u| (u.name.as_str(), u)).collect(),
            interfaces: BTreeMap::new(),
            rust_names: BTreeMap::new(),
            ability_fields: BTreeSet::new(),
            packets: ir.packets.iter().map(|p| p.ty.as_str()).collect(),
            limits,
        };
        model.collect_unions()?;
        model.collect_names()?;
        for key in SHARED_FIELDS {
            let (go, field) = key.rsplit_once('.').ok_or("shared field without a type")?;
            let ty = model
                .struct_def(go)
                .and_then(|d| d.fields.iter().find(|f| f.name == field))
                .map(|f| &f.ty)
                .ok_or_else(|| format!("SHARED_FIELDS lists {key}, which the IR does not have"))?;
            let codec = ty.kind == "named" && model.has_codec(&ty.name);
            if !codec && !NBT_FIELDS.contains(key) {
                return Err(format!(
                    "shared field {key} is neither a type with a Codec nor an NBT field"
                ));
            }
        }
        for t in &ir.types {
            if let Some(body) = &t.marshal {
                collect_ability_fields(
                    &t.name,
                    &body.receiver,
                    &body.stmts,
                    &mut model.ability_fields,
                );
            }
        }
        Ok(model)
    }

    /// The hand method of a function, if it is hand-written.
    pub(super) fn hand_function(go: &str) -> Option<&'static str> {
        HAND_FUNCTIONS
            .iter()
            .find(|(g, _)| *g == go)
            .map(|(_, r)| *r)
    }

    /// Whether a `marshal` statement may name this type.
    pub(super) fn has_codec(&self, go: &str) -> bool {
        if hand_type(go).is_some() {
            return HAND_CODECS.contains(&go);
        }
        self.types.get(go).is_some_and(|t| t.marshal.is_some())
    }

    /// Whether a union variant or packet type is boxed: its estimated size is past
    /// [`BOX_OVER`].
    pub(super) fn is_boxed(&self, go: &str) -> bool {
        self.types
            .get(go)
            .is_some_and(|d| self.struct_bytes(d) > BOX_OVER)
    }

    /// Whether the Rust type of a Go type is `Copy`.
    pub(super) fn is_copy(&self, t: &Type) -> bool {
        match t.kind.as_str() {
            "basic" => t.name != "string",
            "named" => {
                if let Some((_, copy)) = hand_type(&t.name) {
                    return copy;
                }
                self.types.get(t.name.as_str()).is_some_and(|d| {
                    d.kind == "array" && d.underlying.as_ref().is_some_and(|u| self.is_copy(u))
                })
            }
            "external" => EXTERNAL_TYPES
                .iter()
                .any(|(g, _, copy)| *g == t.name && *copy),
            "array" | "optional" => t.elem.as_deref().is_some_and(|e| self.is_copy(e)),
            "any" => true,
            _ => false,
        }
    }

    /// The Rust type of a Go type, written from `from`.
    pub(super) fn rust_type(&self, t: &Type, from: Module) -> Result<String, String> {
        Ok(match t.kind.as_str() {
            "basic" => basic(&t.name)?.to_owned(),
            "named" => self.named(&t.name, from)?,
            "external" => EXTERNAL_TYPES
                .iter()
                .find(|(g, _, _)| *g == t.name)
                .map(|(_, r, _)| (*r).to_owned())
                .ok_or_else(|| format!("unknown external type {}", t.name))?,
            "slice" => {
                let elem = elem(t)?;
                if elem.kind == "any" {
                    return Err(format!("unsupported slice of any {t:?}"));
                }
                format!("Vec<{}>", self.rust_type(elem, from)?)
            }
            "array" => format!(
                "[{}; {}]",
                self.rust_type(elem(t)?, from)?,
                t.len.ok_or("array without length")?
            ),
            "map" => {
                let key = t.key.as_deref().ok_or("map without key")?;
                if key.kind == "basic" && key.name == "string" && elem(t)?.kind == "any" {
                    "nbt::Compound".to_owned()
                } else {
                    return Err(format!("unsupported map type {t:?}"));
                }
            }
            "optional" => format!("Option<{}>", self.rust_type(elem(t)?, from)?),
            "any" => "AbilityValue".to_owned(),
            _ => return Err(format!("unsupported type {t:?}")),
        })
    }

    /// The Rust name of a named Go type, written from `from`.
    pub(super) fn named(&self, go: &str, from: Module) -> Result<String, String> {
        if let Some((rust, _)) = hand_type(go) {
            return Ok(self.qualify(rust, Module::Types, from));
        }
        if let Some(info) = self.interfaces.get(go) {
            return Ok(self.qualify(&info.rust, Module::Types, from));
        }
        let (rust, module) = self
            .rust_names
            .get(go)
            .ok_or_else(|| format!("unknown type {go}"))?;
        Ok(self.qualify(rust, *module, from))
    }

    /// The fields of a named struct type.
    pub(super) fn struct_def(&self, go: &str) -> Option<&'a TypeDef> {
        self.types.get(go).copied().filter(|t| t.kind == "struct")
    }

    /// `name` as written from `from`: a protocol type that a packet item shadows is written
    /// with `super::`.
    fn qualify(&self, name: &str, module: Module, from: Module) -> String {
        if module == Module::Types
            && from == Module::Packet
            && self
                .rust_names
                .values()
                .any(|(n, m)| *m == Module::Packet && n == name)
        {
            format!("super::{name}")
        } else {
            name.to_owned()
        }
    }

    /// An estimate of the size in bytes of the Rust form of a Go type, for boxing decisions.
    fn size(&self, t: &Type) -> u64 {
        match t.kind.as_str() {
            "basic" => match t.name.as_str() {
                "bool" | "uint8" | "int8" => 1,
                "uint16" | "int16" => 2,
                "uint32" | "int32" | "float32" => 4,
                "string" => 24,
                _ => 8,
            },
            "named" => {
                if let Some(info) = self.interfaces.get(&t.name) {
                    let largest = info
                        .variants
                        .iter()
                        .map(|(g, _)| self.variant_size(g))
                        .max()
                        .unwrap_or(0);
                    return largest + 8;
                }
                if let Some(bytes) = hand_size(&t.name) {
                    return bytes;
                }
                self.types.get(t.name.as_str()).map_or(8, |d| {
                    if let Some(u) = &d.underlying {
                        self.size(u)
                    } else {
                        self.struct_size(d)
                    }
                })
            }
            "external" => match t.name.as_str() {
                "mgl32.Vec3" => 12,
                "mgl32.Vec2" => 8,
                "uuid.UUID" => 16,
                _ => 4,
            },
            "array" => {
                t.len.unwrap_or(0).unsigned_abs() * t.elem.as_deref().map_or(0, |e| self.size(e))
            }
            "optional" => {
                let e = t.elem.as_deref().map_or(0, |e| self.size(e));
                let niche = t.elem.as_deref().is_some_and(|e| {
                    matches!(e.kind.as_str(), "slice" | "map")
                        || (e.kind == "basic" && matches!(e.name.as_str(), "string" | "bool"))
                });
                if niche { e } else { e + e.clamp(1, 8) }
            }
            "any" => 8,
            _ => 24,
        }
    }

    /// The size estimate of a union variant as stored in its enum (boxed past the limit).
    fn variant_size(&self, go: &str) -> u64 {
        if self.is_boxed(go) {
            return 8;
        }
        self.types.get(go).map_or(0, |d| self.struct_size(d))
    }

    fn struct_size(&self, d: &TypeDef) -> u64 {
        self.struct_bytes(d).div_ceil(8) * 8
    }

    /// The summed size estimates of the fields of `d`, before alignment.
    fn struct_bytes(&self, d: &TypeDef) -> u64 {
        d.fields.iter().map(|f| self.field_size(d, f)).sum()
    }

    /// The size estimate of a field of `d`: a pointer for a shared field.
    fn field_size(&self, d: &TypeDef, f: &Field) -> u64 {
        if is_shared(&d.name, &f.name) {
            8
        } else {
            self.size(&f.ty)
        }
    }

    fn collect_names(&mut self) -> Result<(), String> {
        let hand: BTreeSet<&str> = hand_names().collect();
        let mut seen: BTreeMap<(Module, String), String> = BTreeMap::new();
        for t in &self.ir.types {
            if hand_type(&t.name).is_some() {
                continue;
            }
            let module = if self.packets.contains(t.name.as_str()) || package(&t.name) == "packet" {
                Module::Packet
            } else {
                Module::Types
            };
            let rust = names::type_name(&t.name)?;
            if module == Module::Types && hand.contains(rust.as_str()) {
                return Err(format!(
                    "{} converts to {rust}, the name of a hand type",
                    t.name
                ));
            }
            if let Some(other) = seen.insert((module, rust.clone()), t.name.clone()) {
                return Err(format!("{other} and {} both convert to {rust}", t.name));
            }
            self.rust_names.insert(t.name.clone(), (rust, module));
        }
        Ok(())
    }

    fn collect_unions(&mut self) -> Result<(), String> {
        for u in &self.ir.unions {
            if u.key != "id" {
                continue;
            }
            let mut go = Vec::new();
            for entry in &u.entries {
                let [Some(variant)] = entry.variants.as_slice() else {
                    return Err(format!("{}: an id row must give one variant", u.name));
                };
                if !go.contains(variant) {
                    go.push(variant.clone());
                }
            }
            let locals: Vec<&str> = go.iter().map(|g| local(g)).collect();
            let rust_variants = names::variants(&locals);
            let rust = names::type_name(&u.interface)?;
            if self.interfaces.contains_key(&u.interface) {
                return Err(format!("{}: two id lookups", u.interface));
            }
            self.interfaces.insert(
                u.interface.clone(),
                UnionInfo {
                    rust,
                    variants: go.into_iter().zip(rust_variants).collect(),
                    by_id: u.name.clone(),
                    by_type: Vec::new(),
                },
            );
        }
        for u in &self.ir.unions {
            if u.key == "type" {
                let info = self
                    .interfaces
                    .get_mut(&u.interface)
                    .ok_or_else(|| format!("{}: no id lookup for {}", u.name, u.interface))?;
                info.by_type.push(u.name.clone());
            }
        }
        for t in &self.ir.types {
            if t.kind == "interface" && !self.interfaces.contains_key(&t.name) {
                return Err(format!("interface {} has no union table", t.name));
            }
        }
        Ok(())
    }
}

/// The Rust primitive of a Go basic type.
pub(super) fn basic(name: &str) -> Result<&'static str, String> {
    Ok(match name {
        "bool" | "untyped bool" => "bool",
        "uint8" => "u8",
        "int8" => "i8",
        "uint16" => "u16",
        "int16" => "i16",
        "uint32" => "u32",
        "int32" => "i32",
        "uint64" | "uint" => "u64",
        "int64" | "int" | "untyped int" => "i64",
        "float32" => "f32",
        "float64" | "untyped float" => "f64",
        "string" | "untyped string" => "String",
        _ => return Err(format!("unsupported basic type {name}")),
    })
}

/// Records the struct fields read or written with `io.AbilityValue`.
fn collect_ability_fields(
    ty: &str,
    receiver: &str,
    stmts: &[Stmt],
    out: &mut BTreeSet<(String, String)>,
) {
    for s in stmts {
        if s.kind == "io"
            && s.method == "AbilityValue"
            && let Some(place) = &s.place
            && place.kind == "field"
            && place
                .x
                .as_deref()
                .is_some_and(|x| x.kind == "var" && x.name == receiver)
        {
            out.insert((ty.to_owned(), place.name.clone()));
        }
        for block in s.blocks() {
            collect_ability_fields(ty, receiver, block, out);
        }
    }
}

/// The element type of a slice, array, map or optional type.
pub(super) fn elem(t: &Type) -> Result<&Type, String> {
    t.elem
        .as_deref()
        .ok_or_else(|| format!("{t:?} has no element type"))
}

/// The Rust names of the hand-written types.
pub(super) fn hand_names() -> impl Iterator<Item = &'static str> {
    HAND_TYPES.iter().map(|(_, rust, _, _)| *rust)
}

/// The Rust names of the hand-written types with their sizes in bytes.
pub(super) fn hand_sizes() -> impl Iterator<Item = (&'static str, u64)> {
    HAND_TYPES.iter().map(|(_, rust, _, bytes)| (*rust, *bytes))
}

/// The hand type of a Go type.
pub(super) fn hand_type(go: &str) -> Option<(&'static str, bool)> {
    hand_row(go).map(|(_, rust, copy, _)| (*rust, *copy))
}

/// The size in bytes of the hand type of a Go type.
fn hand_size(go: &str) -> Option<u64> {
    hand_row(go).map(|(_, _, _, bytes)| *bytes)
}

/// The [`HAND_TYPES`] row of a Go type.
fn hand_row(go: &str) -> Option<&'static (&'static str, &'static str, bool, u64)> {
    HAND_TYPES.iter().find(|(g, _, _, _)| *g == go)
}

/// The IO method with the writer taking its value by `pass` and not failing; its reader returns
/// the value.
const fn io(rust: &'static str, pass: Pass) -> IoSpec {
    IoSpec {
        rust,
        pass,
        write_fails: false,
        read: Read::Returns,
    }
}

/// [`io`] with a writer that fails.
const fn io_fallible(rust: &'static str, pass: Pass) -> IoSpec {
    IoSpec {
        rust,
        pass,
        write_fails: true,
        read: Read::Returns,
    }
}

/// The Rust form of an IO method.
pub(super) fn io_spec(go: &str) -> Option<IoSpec> {
    IO_METHODS.iter().find(|(g, _)| *g == go).map(|(_, s)| *s)
}

/// Whether field `field` of Go type `go` is held as `Arc<T>` ([`SHARED_FIELDS`]).
pub(super) fn is_shared(go: &str, field: &str) -> bool {
    SHARED_FIELDS
        .iter()
        .any(|k| k.strip_prefix(go).and_then(|f| f.strip_prefix('.')) == Some(field))
}

/// The unqualified part of a Go name.
pub(super) fn local(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, n)| n)
}

/// The module a Go name's items go to.
pub(super) fn module_of(go: &str) -> Module {
    if package(go) == "packet" {
        Module::Packet
    } else {
        Module::Types
    }
}

/// The package of a qualified Go name.
fn package(name: &str) -> &str {
    name.split_once('.').map_or("", |(p, _)| p)
}

/// Rewrites each field of [`NBT_FIELDS`] in `ir` from a byte slice read and written by one
/// top-level `Bytes` statement of its type's `Marshal` to a network little-endian `NBT` compound.
/// Fails when the IR no longer has that shape.
pub(super) fn retype_nbt_fields(ir: &mut Ir) -> Result<(), String> {
    let compound: Type = serde_json::from_str(NBT_TYPE).map_err(|e| format!("NBT_TYPE: {e}"))?;
    let network: Expr =
        serde_json::from_str(NBT_NETWORK_ARG).map_err(|e| format!("NBT_NETWORK_ARG: {e}"))?;
    for key in NBT_FIELDS {
        let (go, field) = key.rsplit_once('.').ok_or("NBT field without a type")?;
        let def = ir
            .types
            .iter_mut()
            .find(|t| t.name == go && t.kind == "struct")
            .ok_or_else(|| format!("NBT_FIELDS lists {key}, whose type the IR does not have"))?;
        let f = def
            .fields
            .iter_mut()
            .find(|f| f.name == field)
            .ok_or_else(|| format!("NBT_FIELDS lists {key}, which the IR does not have"))?;
        let is_bytes = f.ty.kind == "slice"
            && f.ty
                .elem
                .as_deref()
                .is_some_and(|e| e.kind == "basic" && e.name == "uint8");
        if !is_bytes {
            return Err(format!("NBT field {key} is not a byte slice"));
        }
        f.ty = compound.clone();
        let stmt = def
            .marshal
            .iter_mut()
            .flat_map(|b| b.stmts.iter_mut())
            .find(|s| {
                s.kind == "io"
                    && s.method == "Bytes"
                    && s.place
                        .as_ref()
                        .is_some_and(|p| p.kind == "field" && p.name == field)
            })
            .ok_or_else(|| format!("NBT field {key} has no top-level Bytes statement"))?;
        "NBT".clone_into(&mut stmt.method);
        if let Some(place) = &mut stmt.place {
            place.ty = Some(compound.clone());
        }
        stmt.args = vec![network.clone()];
    }
    Ok(())
}
