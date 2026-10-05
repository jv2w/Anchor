//! What the generator knows besides the IR: the hand-written Rust types and IO methods, the
//! Go type to Rust type mapping, and lookups over the IR.

use std::collections::{BTreeMap, BTreeSet};

use super::ir::{Enum, Function, Ir, Stmt, Type, TypeDef, Union};
use super::names;

/// Go types with a hand-written Rust type (`src/protocol/*.rs`): Go name, Rust name, and
/// whether the Rust type is `Copy`.
const HAND_TYPES: &[(&str, &str, bool)] = &[
    ("protocol.BlockPos", "BlockPos", true),
    ("protocol.ChunkPos", "ChunkPos", true),
    ("protocol.SubChunkPos", "SubChunkPos", true),
    ("protocol.Bitset", "Bitset", false),
    ("protocol.EntityMetadata", "EntityMetadata", false),
    ("protocol.ItemType", "ItemType", true),
    ("protocol.ItemStack", "ItemStack", false),
    ("protocol.ItemInstance", "ItemInstance", false),
    ("protocol.StackRequestItem", "StackRequestItem", false),
    ("protocol.MaterialReducer", "MaterialReducer", false),
    (
        "protocol.MaterialReducerOutput",
        "MaterialReducerOutput",
        true,
    ),
    ("protocol.GameRule", "GameRule", false),
    ("protocol.PackSetting", "PackSetting", false),
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

/// External Go types: Go name, Rust type, `Copy`.
const EXTERNAL_TYPES: &[(&str, &str, bool)] = &[
    ("mgl32.Vec3", "[f32; 3]", true),
    ("mgl32.Vec2", "[f32; 2]", true),
    ("uuid.UUID", "Uuid", true),
    ("color.RGBA", "Rgba", true),
];

/// Functions written by hand in `src/protocol/union_frame.rs` as `Reader`/`Writer` methods,
/// with the method name. A Go union value is a pointer, so a body that copies one into a local
/// and changes it through the copy has no value-type translation.
const HAND_FUNCTIONS: &[(&str, &str)] = &[(
    "protocol.StackRequestItemDescriptorCount",
    "stack_request_item_descriptor_count",
)];

/// How a writer takes an IO method's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// By value (`Copy` values).
    Value,
    /// By reference.
    Ref,
}

/// How a reader produces an IO method's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    /// `r.method(args)?` returns the value.
    Returns,
    /// `r.method(&place)?` checks the current value and returns nothing.
    Checks,
}

/// One `protocol.IO` method in Rust.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IoSpec {
    /// The Rust method name on `Reader` and `Writer`.
    pub(crate) rust: &'static str,
    /// How the writer takes the value.
    pub(crate) pass: Pass,
    /// Whether the writer returns `Result`.
    pub(crate) write_fails: bool,
    /// How the reader produces the value.
    pub(crate) read: Read,
}

const fn io(rust: &'static str, pass: Pass, write_fails: bool) -> IoSpec {
    IoSpec {
        rust,
        pass,
        write_fails,
        read: Read::Returns,
    }
}

/// The `protocol.IO` methods and their Rust form (`src/protocol/io.rs` names them; the writer
/// signatures decide `pass` and `write_fails`).
const IO_METHODS: &[(&str, IoSpec)] = &[
    ("AbilityValue", io("ability_value", Pass::Value, false)),
    ("ActorRuntimeID", io("actor_runtime_id", Pass::Value, false)),
    (
        "ActorRuntimeIDVarint64",
        io("actor_runtime_id_varint64", Pass::Value, false),
    ),
    (
        "ActorRuntimeIDVaruint32",
        io("actor_runtime_id_varuint32", Pass::Value, false),
    ),
    ("ActorUniqueID", io("actor_unique_id", Pass::Value, false)),
    (
        "ActorUniqueIDInt64",
        io("actor_unique_id_int64", Pass::Value, false),
    ),
    (
        "ActorUniqueIDUint64",
        io("actor_unique_id_uint64", Pass::Value, false),
    ),
    (
        "ActorUniqueIDVaruint64",
        io("actor_unique_id_varuint64", Pass::Value, false),
    ),
    ("BEARGB", io("be_argb", Pass::Value, false)),
    ("BEInt32", io("be_int32", Pass::Value, false)),
    ("Bitset", io("bitset", Pass::Ref, true)),
    ("BlockPos", io("block_pos", Pass::Value, false)),
    ("Bool", io("bool", Pass::Value, false)),
    ("ByteFloat", io("byte_float", Pass::Value, false)),
    ("ByteSlice", io("byte_slice", Pass::Ref, true)),
    ("Bytes", io("bytes", Pass::Ref, false)),
    ("ChunkPos", io("chunk_pos", Pass::Value, false)),
    ("EntityMetadata", io("entity_metadata", Pass::Ref, true)),
    (
        "EventOrdinal",
        IoSpec {
            rust: "event_ordinal",
            pass: Pass::Ref,
            write_fails: true,
            read: Read::Checks,
        },
    ),
    ("EventType", io("event_type", Pass::Ref, false)),
    ("Float32", io("float32", Pass::Value, false)),
    ("Float64", io("float64", Pass::Value, false)),
    ("GameRule", io("game_rule", Pass::Ref, true)),
    ("Int16", io("int16", Pass::Value, false)),
    ("Int32", io("int32", Pass::Value, false)),
    ("Int64", io("int64", Pass::Value, false)),
    ("Int8", io("int8", Pass::Value, false)),
    ("Item", io("item", Pass::Ref, true)),
    (
        "ItemDescriptorCount",
        io("item_descriptor_count", Pass::Ref, true),
    ),
    ("ItemInstance", io("item_instance", Pass::Ref, true)),
    ("MaterialReducer", io("material_reducer", Pass::Ref, true)),
    ("NBT", io("nbt", Pass::Ref, true)),
    ("PackSetting", io("pack_setting", Pass::Ref, true)),
    (
        "PlayerInventoryAction",
        io("player_inventory_action", Pass::Ref, true),
    ),
    ("RGB", io("rgb", Pass::Value, false)),
    ("RGBA", io("rgba", Pass::Value, false)),
    ("ShapeData", io("shape_data", Pass::Ref, true)),
    ("SoundPos", io("sound_pos", Pass::Value, false)),
    (
        "StackRequestAction",
        io("stack_request_action", Pass::Ref, true),
    ),
    (
        "StackRequestItem",
        io("stack_request_item", Pass::Ref, true),
    ),
    ("String", io("string", Pass::Ref, true)),
    ("StringUTF", io("string_utf", Pass::Ref, true)),
    ("SubChunkPos", io("sub_chunk_pos", Pass::Value, false)),
    (
        "TransactionDataType",
        io("transaction_data_type", Pass::Ref, false),
    ),
    ("UUID", io("uuid", Pass::Value, false)),
    ("Uint16", io("uint16", Pass::Value, false)),
    ("Uint32", io("uint32", Pass::Value, false)),
    ("Uint64", io("uint64", Pass::Value, false)),
    ("Uint8", io("uint8", Pass::Value, false)),
    ("Varint32", io("varint32", Pass::Value, false)),
    ("Varint64", io("varint64", Pass::Value, false)),
    ("Varuint32", io("varuint32", Pass::Value, false)),
    ("Varuint64", io("varuint64", Pass::Value, false)),
    ("Vec2", io("vec2", Pass::Value, false)),
    ("Vec3", io("vec3", Pass::Value, false)),
];

/// IO methods that are not statements: they appear as expressions and `error` statements.
const IO_NON_STATEMENTS: &[&str] = &["ShieldID", "InvalidValue", "UnknownEnumOption"];

/// IO methods that no `Marshal` calls, so they have no Rust form; a call fails generation.
const IO_UNUSED: &[&str] = &["NBTList"];

/// The Rust module a generated item lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Module {
    /// `src/protocol/types.rs`
    Types,
    /// `src/protocol/packet.rs`
    Packet,
}

/// A union interface: its variants and lookup functions.
#[derive(Debug)]
pub(crate) struct UnionInfo {
    /// The Rust enum name.
    pub(crate) rust: String,
    /// Variants in ID table order: Go type and Rust variant name.
    pub(crate) variants: Vec<(String, String)>,
    /// The `id`-key lookup (ID to variant).
    pub(crate) by_id: String,
    /// The `type`-key lookups (variant to ID).
    pub(crate) by_type: Vec<String>,
}

/// Everything the emitter and translator look up.
#[derive(Debug)]
pub(crate) struct Model<'a> {
    pub(crate) ir: &'a Ir,
    pub(crate) types: BTreeMap<&'a str, &'a TypeDef>,
    pub(crate) functions: BTreeMap<&'a str, &'a Function>,
    pub(crate) enums: BTreeMap<&'a str, &'a Enum>,
    pub(crate) unions: BTreeMap<&'a str, &'a Union>,
    /// Union interfaces by Go name.
    pub(crate) interfaces: BTreeMap<String, UnionInfo>,
    /// Go union variant type to its interface.
    pub(crate) variant_of: BTreeMap<String, String>,
    /// Rust names of generated types by Go name.
    pub(crate) rust_names: BTreeMap<String, (String, Module)>,
    /// Struct fields of type `any` written with `AbilityValue`.
    pub(crate) ability_fields: BTreeSet<(String, String)>,
    /// Packet type names.
    pub(crate) packets: BTreeSet<&'a str>,
    /// Decode limits by field (`<Go type>.<field>`, `[]` for its list elements): the most bytes
    /// of a string or elements of a list (`xtask/data/reference/protocol_limits.json`).
    pub(crate) limits: BTreeMap<String, usize>,
}

/// The package of a qualified Go name.
pub(crate) fn package(name: &str) -> &str {
    name.split_once('.').map_or("", |(p, _)| p)
}

/// The module a Go name's items go to.
pub(crate) fn module_of(go: &str) -> Module {
    if package(go) == "packet" {
        Module::Packet
    } else {
        Module::Types
    }
}

/// The unqualified part of a Go name.
pub(crate) fn local(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, n)| n)
}

impl<'a> Model<'a> {
    pub(crate) fn new(ir: &'a Ir, limits: BTreeMap<String, usize>) -> Result<Self, String> {
        let names: Vec<&str> = ir.io_methods.iter().map(|m| m.name.as_str()).collect();
        for name in &names {
            if !IO_NON_STATEMENTS.contains(name)
                && !IO_UNUSED.contains(name)
                && io_spec(name).is_none()
            {
                return Err(format!("IO method {name} has no Rust form in IO_METHODS"));
            }
        }
        for (name, _) in IO_METHODS {
            if !names.contains(name) {
                return Err(format!(
                    "IO_METHODS lists {name}, which the IR does not have"
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
            variant_of: BTreeMap::new(),
            rust_names: BTreeMap::new(),
            ability_fields: BTreeSet::new(),
            packets: ir.packets.iter().map(|p| p.ty.as_str()).collect(),
            limits,
        };
        model.collect_unions()?;
        model.collect_names()?;
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
            for g in &go {
                self.variant_of.insert(g.clone(), u.interface.clone());
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

    fn collect_names(&mut self) -> Result<(), String> {
        let hand: BTreeSet<&str> = HAND_TYPES.iter().map(|(_, r, _)| *r).collect();
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

    /// The Rust name of a named Go type, written from `from`.
    pub(crate) fn named(&self, go: &str, from: Module) -> Result<String, String> {
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

    /// The Rust type of a Go type, written from `from`.
    pub(crate) fn rust_type(&self, t: &Type, from: Module) -> Result<String, String> {
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

    /// Whether the Rust type of a Go type is `Copy`.
    pub(crate) fn is_copy(&self, t: &Type) -> bool {
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

    /// Whether a `marshal` statement may name this type.
    pub(crate) fn has_codec(&self, go: &str) -> bool {
        if hand_type(go).is_some() {
            return HAND_CODECS.contains(&go);
        }
        self.types.get(go).is_some_and(|t| t.marshal.is_some())
    }

    /// The hand method of a function, if it is hand-written.
    pub(crate) fn hand_function(go: &str) -> Option<&'static str> {
        HAND_FUNCTIONS
            .iter()
            .find(|(g, _)| *g == go)
            .map(|(_, r)| *r)
    }

    /// The fields of a named struct type.
    pub(crate) fn struct_def(&self, go: &str) -> Option<&'a TypeDef> {
        self.types.get(go).copied().filter(|t| t.kind == "struct")
    }

    /// An estimate of the size in bytes of the Rust form of a Go type, for boxing decisions.
    pub(crate) fn size(&self, t: &Type) -> u64 {
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
                match t.name.as_str() {
                    "protocol.BlockPos" | "protocol.SubChunkPos" => 12,
                    "protocol.ChunkPos"
                    | "protocol.ItemType"
                    | "protocol.MaterialReducerOutput" => 8,
                    "protocol.ItemStack" => 112,
                    "protocol.ItemInstance" => 120,
                    "protocol.StackRequestItem" => 136,
                    "protocol.GameRule" => 40,
                    "protocol.PackSetting" => 56,
                    "protocol.Bitset" | "protocol.EntityMetadata" | "protocol.MaterialReducer" => {
                        32
                    }
                    name => self.types.get(name).map_or(8, |d| {
                        if let Some(u) = &d.underlying {
                            self.size(u)
                        } else {
                            self.struct_size(d)
                        }
                    }),
                }
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
    pub(crate) fn variant_size(&self, go: &str) -> u64 {
        let size = self.types.get(go).map_or(0, |d| self.struct_size(d));
        if size > BOX_OVER { 8 } else { size }
    }

    fn struct_size(&self, d: &TypeDef) -> u64 {
        let sum: u64 = d.fields.iter().map(|f| self.size(&f.ty)).sum();
        sum.div_ceil(8) * 8
    }
}

/// Union variants and packets larger than this many bytes (estimated) are boxed, so the enums
/// stay small to move and `clippy::large_enum_variant` holds.
pub(crate) const BOX_OVER: u64 = 128;

/// The Rust names of the hand-written types.
pub(crate) fn hand_names() -> impl Iterator<Item = &'static str> {
    HAND_TYPES.iter().map(|(_, r, _)| *r)
}

/// The hand type of a Go type.
pub(crate) fn hand_type(go: &str) -> Option<(&'static str, bool)> {
    HAND_TYPES
        .iter()
        .find(|(g, _, _)| *g == go)
        .map(|(_, r, c)| (*r, *c))
}

/// The Rust form of an IO method.
pub(crate) fn io_spec(go: &str) -> Option<IoSpec> {
    IO_METHODS.iter().find(|(g, _)| *g == go).map(|(_, s)| *s)
}

/// The Rust primitive of a Go basic type.
pub(crate) fn basic(name: &str) -> Result<&'static str, String> {
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

fn elem(t: &Type) -> Result<&Type, String> {
    t.elem
        .as_deref()
        .ok_or_else(|| format!("{t:?} has no element type"))
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
        for list in [&s.body, &s.then, &s.els, &s.init, &s.post] {
            collect_ability_fields(ty, receiver, list, out);
        }
        for c in &s.cases {
            collect_ability_fields(ty, receiver, &c.body, out);
        }
    }
}
