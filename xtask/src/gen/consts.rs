//! `src/protocol/consts.rs`: the named protocol constants from the const
//! declarations of the gophertunnel `protocol` and `packet` packages in the IR.
//!
//! [`SPECS`] names every constant group. A constant belongs to the group of its package whose
//! prefix is the longest one its name starts with (at a word start); all constants of one Go
//! declaration must land in one group, and every group must get a constant, so a constant or a
//! declaration the table does not describe fails the generation. A closed set becomes an enum
//! with gophertunnel's values as discriminants and a fallible conversion from its integer (or
//! string), flags become `Flag` enums for `FlagSet`, open sets become plain constants, and
//! groups the generated codecs already stand for are left out with the reason. The generated
//! tests compare every Rust value with the IR, so a constant changed by a gophertunnel update
//! fails `cargo test` until `cargo xtask gen` runs again.

use std::collections::{BTreeMap, BTreeSet};

use super::body::{int_lit, str_lit};
use super::emit::{HEADER, doc};
use super::ir::{DeclConst, Ir};
use super::names;

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/protocol/consts.rs";

/// What a group becomes.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// A closed set of integers or strings: an enum.
    Enum,
    /// Flags whose values are masks (`1 << bit`): an enum and a `Flag` implementation.
    Mask,
    /// Flags whose values are bit positions: an enum and a `Flag` implementation.
    Index,
    /// An open set or single values: plain constants.
    Values,
    /// The tags of a generated union, whose enum stands for them (`from_id`, `id`).
    UnionTag,
    /// The same names and values as the group with this key.
    SameAs(&'static str),
    /// The bit positions of the flags of the mask group with this key (`Flag::bit`).
    BitsOf(&'static str),
    /// Left out for this reason.
    Excluded(&'static str),
}

/// One constant group.
#[derive(Debug, Clone, Copy)]
struct Spec {
    /// The package and the Go name of the group (`packet.InputFlag`); the Rust type name
    /// converts from it ([`names::type_name`]).
    key: &'static str,
    kind: Kind,
    /// The Rust integer type of untyped integer constants; empty for the kind's default.
    /// Typed constants keep their Go type.
    repr: &'static str,
    /// The name prefixes; empty for the name of the key.
    prefixes: &'static [&'static str],
    /// Names (after the prefix) of count markers: `max + 1` for an enum or index flags, the mask
    /// after the last one for mask flags.
    sentinels: &'static [&'static str],
    /// Variants keep the whole Go name (the prefixes only select the group).
    whole: bool,
}

const fn s(key: &'static str, kind: Kind) -> Spec {
    Spec {
        key,
        kind,
        repr: "",
        prefixes: &[],
        sentinels: &[],
        whole: false,
    }
}

impl Spec {
    const fn repr(self, repr: &'static str) -> Self {
        Self { repr, ..self }
    }
    const fn prefixes(self, prefixes: &'static [&'static str]) -> Self {
        Self { prefixes, ..self }
    }
    const fn count(self) -> Self {
        Self {
            sentinels: &["Count"],
            ..self
        }
    }
    const fn sentinels(self, sentinels: &'static [&'static str]) -> Self {
        Self { sentinels, ..self }
    }
    const fn whole(self) -> Self {
        Self {
            whole: true,
            ..self
        }
    }
}

use Kind::{BitsOf, Enum, Excluded, Index, Mask, SameAs, UnionTag, Values};

/// Every constant group, in the order of the generated file. `repr` follows the packet field
/// that carries the group (the one of the current packet when several do).
const SPECS: &[Spec] = &[
    // protocol
    s("protocol.Ability", Mask).repr("u32").count(),
    s("protocol.AbilityLayerType", Enum).repr("u16"),
    s("protocol.AbilityBase", Values),
    s("protocol.AttributeModifierOperation", Enum),
    s("protocol.AttributeModifierOperand", Enum),
    s("protocol.AttributeLayerPayloadType", Enum).repr("u32"),
    s("protocol.AttributeDataType", Enum).repr("u32"),
    s("protocol.AttributeBoolOperation", Enum),
    s("protocol.AttributeFloatOperation", Enum),
    s("protocol.AttributeColourOperation", Enum),
    s("protocol.NoiseAlignmentType", Enum).repr("u8"),
    s("protocol.BiomeExpressionOp", Enum)
        .prefixes(&["BiomeExpressionOp", "BiomeExpression"])
        .count(),
    s("protocol.BiomeCoordinateEvaluationOrder", Enum),
    s("protocol.BiomeRandomDistributionType", Enum),
    s("protocol.AimAssistTargetMode", Enum),
    s("protocol.AudioListener", Enum).repr("u8"),
    s("protocol.EasingType", Enum),
    s("protocol.SplineType", Enum),
    s("protocol.ClockPayloadType", Enum).repr("u32"),
    s("protocol.CommandPermissionLevel", Enum).repr("u8"),
    s("protocol.CommandArg", Mask).repr("u32"),
    s("protocol.CommandArgType", Enum).repr("u32"),
    s("protocol.ParamOption", Mask).repr("u8"),
    s("protocol.CommandEnumConstraint", Enum).repr("u8"),
    s("protocol.CommandOrigin", Enum).repr("u32"),
    s("protocol.Container", Enum).repr("u8"),
    s("protocol.ContainerType", Enum).repr("u8"),
    s("protocol.CreativeCategory", Enum).repr("u8"),
    s("protocol.DataStoreChangeType", Enum).repr("u32"),
    s("protocol.DataStoreControl", Enum).repr("u32"),
    s("protocol.DataStorePropertyType", Enum),
    s("protocol.EntityLink", Enum).repr("u8"),
    s("protocol.EntityDataKey", Enum).repr("u32"),
    s("protocol.EntityDataFlag", Index).repr("u32").count(),
    s("protocol.EntityDataType", Enum),
    s("protocol.EventType", UnionTag),
    s("protocol.CopperBlockID", Enum)
        .prefixes(&["Wax", "UnWax"])
        .whole(),
    s("protocol.FurnaceLayout", Enum),
    s("protocol.FurnaceLeftTab", Enum),
    s("protocol.GraphicsOverrideParameterType", Enum),
    s(
        "protocol.Current",
        Excluded("packet::PROTOCOL_VERSION and packet::GAME_VERSION"),
    ),
    s("protocol.InventoryActionSource", Enum).repr("u32"),
    s("protocol.WindowID", Values).repr("u32"),
    s("protocol.InventoryTransactionType", UnionTag),
    s("protocol.UseItemAction", Enum).repr("u32"),
    s("protocol.TriggerType", Enum).repr("u32"),
    s("protocol.ClientPrediction", Enum).repr("u8"),
    s("protocol.ClientCooldownState", Enum).repr("u8"),
    s("protocol.HandSlot", Enum).repr("u8"),
    s("protocol.UseItemOnEntityAction", Enum),
    s("protocol.ReleaseItemAction", Enum),
    s("protocol.ItemEntryVersion", Enum),
    s("protocol.ItemDescriptor", UnionTag),
    s("protocol.FilterCause", Enum),
    s("protocol.ItemStackResponseStatus", Enum).repr("u8"),
    s("protocol.StackRequestAction", UnionTag),
    s("protocol.MapDecorationType", Enum).repr("u8"),
    s("protocol.MapObjectType", Enum),
    s("protocol.MemoryCategory", Enum).repr("u8"),
    s("protocol.DeviceOS", Enum).prefixes(&["Device"]),
    s("protocol.PlayerListAction", Enum).repr("u8"),
    s("protocol.PlayerAction", Enum).count(),
    s("protocol.RecipeUnlockContext", Enum),
    s("protocol.PackSettingType", Enum).repr("u32"),
    s("protocol.ScoreboardIdentity", Enum).repr("u8"),
    s("protocol.ShapeData", UnionTag),
    s("protocol.PrimitiveShape", Enum),
    s("protocol.ArmSize", Enum).repr("u8"),
    s("protocol.SkinAnimation", Enum).repr("u32"),
    s("protocol.ExpressionType", Enum).repr("u32"),
    s("protocol.PieceType", Enum).repr("u32"),
    s("protocol.SoundDataUpdate", Enum),
    s("protocol.StructureMirror", Enum).repr("u8"),
    s("protocol.StructureRotation", Enum).repr("u8"),
    s("protocol.AnimationMode", Enum).repr("u8"),
    s("protocol.HeightMapData", Enum).repr("u8"),
    s("protocol.SubChunkResult", Enum).repr("u8"),
    s("protocol.WaypointAction", Enum).repr("u8"),
    s("protocol.WaypointUpdateFlag", Mask).repr("u32"),
    s("protocol.WaypointTexture", Enum),
    s("protocol.Generator", Enum),
    // packet
    s("packet.ActorEvent", Enum).repr("u8"),
    s("packet.AdventureFlag", Mask)
        .repr("u32")
        .prefixes(&["AdventureFlag", "AdventureSettingsFlags"]),
    s("packet.ActionPermission", Mask).repr("u32"),
    s("packet.PermissionLevel", Enum).repr("u8"),
    s("packet.AgentActionType", Enum),
    s("packet.AnimateAction", Enum).repr("u8"),
    s("packet.AnimateSwingSource", Enum).repr("u8"),
    s("packet.BlockEventType", Enum).prefixes(&["BlockEvent"]),
    s("packet.BookAction", Enum).repr("u32"),
    s("packet.BossEventType", Enum)
        .repr("u8")
        .prefixes(&["BossEvent"]),
    s("packet.BossEventColour", Enum).repr("u8"),
    s("packet.BossEventOverlay", Enum).repr("u8"),
    s("packet.CameraAimAssistAction", Enum).repr("u8"),
    s("packet.CameraAunAssistPresetOperation", Enum).repr("u8"),
    s("packet.CameraShakeType", Enum),
    s("packet.CameraShakeAction", Enum).repr("u8"),
    s("packet.Dimension", Enum),
    s("packet.ControlScheme", Enum).repr("u8"),
    s("packet.ClientBoundDebugRendererType", Enum)
        .repr("u32")
        .prefixes(&["ClientBoundDebugRenderer"]),
    s("packet.ClientCameraAimAssistAction", Enum).repr("u8"),
    s("packet.TextureShiftAction", Enum).repr("u8"),
    s("packet.CodeBuilderOperation", Enum).repr("u8"),
    s("packet.CodeBuilderCategory", Enum).repr("u8"),
    s("packet.CodeBuilderStatus", Enum).repr("u8"),
    s("packet.CommandBlockMode", Enum)
        .repr("u32")
        .prefixes(&["CommandBlock"]),
    s("packet.CommandOutputType", Enum).repr("u8"),
    s("packet.UseItemMethod", Enum).prefixes(&["UseItem"]),
    s("packet.ContainerDataFurnace", Enum),
    s("packet.ContainerDataBrewingStand", Enum),
    s("packet.PredictionType", Enum).repr("u8"),
    s(
        "packet.DefaultMaxDecompressedLen",
        Excluded("the max-batch-bytes setting"),
    ),
    s("packet.DisconnectReason", Enum),
    s("packet.EmoteFlag", Mask).repr("u8"),
    s("packet.GameTestRequestRotation", Enum)
        .repr("u8")
        .prefixes(&["GameTestRequest"]),
    s("packet.ID", Excluded("packet IDs: the Packet enum")),
    s("packet.InteractAction", Enum).repr("u8"),
    s(
        "packet.InventoryTransactionType",
        SameAs("protocol.InventoryTransactionType"),
    ),
    s("packet.LabTableAction", Enum).repr("u8"),
    s("packet.LessonAction", Enum),
    s("packet.ParticleType", Enum),
    s("packet.LevelEvent", Enum),
    s("packet.SoundEvent", Enum),
    s("packet.MobEffectOperation", Enum)
        .repr("u8")
        .prefixes(&["MobEffect"]),
    s("packet.Effect", Enum),
    s("packet.ModalFormCancelReason", Enum).repr("u8"),
    s("packet.MoveFlag", Mask).repr("u8"),
    s("packet.MoveMode", Enum).repr("u8"),
    s("packet.TeleportCause", Enum),
    s("packet.MovementEffectType", Enum),
    s("packet.MultiPlayerSettingsAction", Enum)
        .prefixes(&["EnableMultiPlayer", "DisableMultiPlayer", "RefreshJoinCode"])
        .whole(),
    s("packet.CompressionAlgorithm", Enum).repr("u16"),
    s("packet.NPCDialogueAction", Enum),
    s("packet.NPCRequestAction", Enum).repr("u8"),
    s("packet.ViolationType", Enum),
    s("packet.ViolationSeverity", Enum),
    s("packet.PhotoType", Enum),
    s("packet.PlayStatus", Enum),
    s("packet.PlayerArmourDamageFlag", Index).repr("u8"),
    s("packet.InputFlag", Index).repr("u32").count(),
    s("packet.InputMode", Enum).repr("u32").count(),
    s("packet.PlayMode", Enum)
        .repr("u32")
        .sentinels(&["NumModes"]),
    s("packet.InteractionModel", Enum).count(),
    s("packet.PlayerLocationType", Enum),
    s("packet.PlayerUpdateEntityOverridesType", Enum).repr("u32"),
    s("packet.PlayerVideoCaptureAction", Enum).repr("u8"),
    s("packet.PositionTrackingDBRequestAction", Enum).repr("u8"),
    s("packet.PositionTrackingDBBroadcastAction", Enum).repr("u8"),
    s("packet.Ability", BitsOf("protocol.Ability")).count(),
    s("packet.PackResponse", Enum).repr("u32"),
    s("packet.ResourcePackType", Enum).repr("u8"),
    s("packet.RespawnState", Enum).repr("u8"),
    s("packet.PartyDestinationCookieIntent", Enum),
    s("packet.LoadingScreenType", Enum),
    s("packet.DataDrivenScreenCloseReason", Enum),
    s("packet.ScoreboardSortOrder", Enum),
    s("packet.ScoreboardSlot", Enum),
    s("packet.HudElement", Enum),
    s("packet.HudVisibility", Enum),
    s("packet.FurnaceType", Enum).repr("u8"),
    s("packet.GameType", Enum),
    s("packet.InventoryLayout", Enum),
    s("packet.InventoryLeftTab", Enum),
    s("packet.InventoryRightTab", Enum),
    s("packet.ScoreboardIdentityAction", Enum).repr("u8"),
    s("packet.SpawnType", Enum),
    s("packet.TitleAction", Enum),
    s("packet.ShowCreditsStatus", Enum),
    s("packet.StoreOfferType", Enum).repr("u8"),
    s("packet.SimpleEvent", Enum).repr("u16"),
    s("packet.SimulationType", Enum),
    s("packet.SpawnBiomeType", Enum).repr("i16"),
    s("packet.XBLBroadcastMode", Enum),
    s("packet.ChatRestrictionLevel", Enum).repr("u8"),
    s("packet.EditorWorldType", Enum),
    s("packet.StructureBlockType", Enum).prefixes(&["StructureBlock"]),
    s("packet.StructureRedstoneSaveMode", Enum).repr("u8"),
    s("packet.StructureTemplateRequestType", Enum)
        .repr("u8")
        .prefixes(&["StructureTemplateRequest"]),
    s("packet.StructureTemplateResponseType", Enum)
        .repr("u8")
        .prefixes(&["StructureTemplateResponse"]),
    s("packet.TextType", Enum).repr("u8"),
    s("packet.TextCategory", Enum),
    s("packet.UnlockedRecipesType", Enum).repr("u32"),
    s("packet.BlockUpdateFlag", Mask)
        .repr("u32")
        .prefixes(&["BlockUpdate"]),
    s("packet.BlockSyncedTransition", Enum)
        .repr("u64")
        .prefixes(&["BlockToEntityTransition", "EntityToBlockTransition"])
        .whole(),
    s("packet.ClientInputLock", Mask).repr("u32"),
    s("packet.GraphicsMode", Enum).repr("u8"),
    s("packet.SoftEnumAction", Enum).repr("u8"),
];

/// Every group key (`packet.TextType`) with the Rust name of its enum, or `None` for a group
/// that is no enum or flag enum, for the field docs of the generated types.
pub(crate) fn enum_names() -> Result<BTreeMap<&'static str, Option<String>>, String> {
    SPECS
        .iter()
        .map(|s| {
            let name = matches!(s.kind, Enum | Mask | Index)
                .then(|| names::type_name(s.key))
                .transpose()?;
            Ok((s.key, name))
        })
        .collect()
}

/// A Rust integer type.
#[derive(Debug, Clone, Copy)]
struct IntType {
    name: &'static str,
    signed: bool,
    bits: u32,
}

impl IntType {
    fn new(name: &str) -> Result<Self, String> {
        let (name, signed, bits) = match name {
            "u8" => ("u8", false, 8),
            "i8" => ("i8", true, 8),
            "u16" => ("u16", false, 16),
            "i16" => ("i16", true, 16),
            "u32" => ("u32", false, 32),
            "i32" => ("i32", true, 32),
            "u64" => ("u64", false, 64),
            "i64" => ("i64", true, 64),
            _ => return Err(format!("`{name}` is not a fixed-size integer type")),
        };
        Ok(Self { name, signed, bits })
    }

    /// The Rust type of a Go basic integer type.
    fn of_go(basic: &str) -> Result<Self, String> {
        let rust = match basic {
            "uint8" => "u8",
            "int8" => "i8",
            "uint16" => "u16",
            "int16" => "i16",
            "uint32" => "u32",
            "int32" => "i32",
            "uint64" => "u64",
            "int64" => "i64",
            _ => return Err(format!("Go type `{basic}` has no fixed-size Rust type")),
        };
        Self::new(rust)
    }

    /// The wire value of a Go value: negative values of unsigned types wrap, as gophertunnel
    /// writes them (`byte(ContainerTypeInventory)` is 255). Fails if it does not fit.
    fn wire(self, value: i128) -> Result<i128, String> {
        let span = 1_i128 << self.bits;
        let value = if !self.signed && value < 0 {
            value + span
        } else {
            value
        };
        let (min, max) = if self.signed {
            (-(span / 2), span / 2 - 1)
        } else {
            (0, span - 1)
        };
        if value < min || value > max {
            return Err(format!("{value} does not fit {}", self.name));
        }
        Ok(value)
    }

    /// The bit width the test wraps Go values to: the width when unsigned, else 0.
    fn unsigned_bits(self) -> u32 {
        if self.signed { 0 } else { self.bits }
    }
}

/// The value kind shared by a group's constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basic {
    Int,
    Float,
    Str,
}

/// One constant of a group.
#[derive(Debug)]
struct Member<'a> {
    c: &'a DeclConst,
    /// The Go name without the package.
    go: &'a str,
    /// The name after the matched prefix (the whole name for [`Spec::whole`]).
    rest: String,
}

/// A group with its constants in declaration order.
#[derive(Debug)]
struct Group<'a> {
    spec: &'static Spec,
    members: Vec<Member<'a>>,
}

/// Generates the file text (unformatted).
pub(crate) fn generate(ir: &Ir) -> Result<String, String> {
    let groups = assign(ir)?;
    let tags = union_tags(ir);
    let mut out = Output::default();
    let mut rust_names = BTreeSet::new();
    for g in &groups {
        let name = names::type_name(g.spec.key)?;
        if matches!(g.spec.kind, Enum | Mask | Index | Values) && !rust_names.insert(name.clone()) {
            return Err(format!("{}: Rust name `{name}` is taken", g.spec.key));
        }
        group_items(g, &name, &groups, &tags, &mut out)
            .map_err(|e| format!("{}: {e}", g.spec.key))?;
    }
    let source = &ir.source;
    let mut text = String::new();
    put!(
        text,
        "{HEADER}\n//! Named protocol constants generated from the const declarations of the protocol IR: \
         gophertunnel {} (commit {}), protocol {}.\n//!\n//! Closed sets are enums with gophertunnel's \
         values as discriminants: `TryFrom` the integer (or string) fails on a value outside the set, \
         `From` gives the integer back. Flag groups implement [`Flag`] for [`FlagSet`](super::FlagSet). \
         Open sets are plain constants.\n\n\
         use super::flags::Flag;\nuse super::io::{{Error, Result}};\n\n{}",
        source.version,
        source.commit,
        source.protocol,
        out.items
    );
    tests(&mut text, &out);
    Ok(text)
}

/// The generated items and test lines.
#[derive(Debug, Default)]
struct Output {
    items: String,
    /// The statements of the test's `values` function: every emitted constant by Go name.
    values: String,
    /// The lines of the test's list of Go names left out.
    excluded: String,
    /// Round-trip test calls.
    checks: Vec<String>,
}

/// Assigns every exported constant to its group and checks the table covers the IR exactly.
fn assign(ir: &Ir) -> Result<Vec<Group<'_>>, String> {
    let mut groups: Vec<Group<'_>> = SPECS
        .iter()
        .map(|spec| Group {
            spec,
            members: Vec::new(),
        })
        .collect();
    for decl in &ir.const_decls {
        let mut target: Option<usize> = None;
        for c in &decl.consts {
            let (package, go) = c
                .name
                .split_once('.')
                .ok_or_else(|| format!("{}: unqualified constant", c.name))?;
            if package != decl.package {
                return Err(format!("{}: declared in package {}", c.name, decl.package));
            }
            let (index, prefix) = best_spec(package, go).ok_or_else(|| {
                format!(
                    "{} ({}): no group in consts::SPECS; add one",
                    c.name, decl.file
                )
            })?;
            if target.is_some_and(|t| t != index) {
                return Err(format!(
                    "{}: one declaration holds constants of two groups ({})",
                    decl.file, c.name
                ));
            }
            target = Some(index);
            let group = groups.get_mut(index).ok_or("group index out of range")?;
            let rest = if group.spec.whole {
                go.to_owned()
            } else {
                go.get(prefix.len()..).unwrap_or_default().to_owned()
            };
            group.members.push(Member { c, go, rest });
        }
    }
    for g in &groups {
        if g.members.is_empty() {
            return Err(format!("{}: no constant has this group", g.spec.key));
        }
    }
    Ok(groups)
}

/// The group (index into [`SPECS`]) and prefix with the longest prefix `go` starts with.
fn best_spec(package: &str, go: &str) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for (i, spec) in SPECS.iter().enumerate() {
        let (p, name) = spec.key.split_once('.')?;
        if p != package {
            continue;
        }
        let own = [name];
        let prefixes: &[&'static str] = if spec.prefixes.is_empty() {
            &own
        } else {
            spec.prefixes
        };
        for &prefix in prefixes {
            let at_word = go.strip_prefix(prefix).is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_none_or(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
            });
            if at_word && best.is_none_or(|(_, b)| b.len() < prefix.len()) {
                best = Some((i, prefix));
            }
        }
    }
    best
}

/// The union interface of each constant used as a union tag, by Go name.
fn union_tags(ir: &Ir) -> BTreeMap<&str, &str> {
    let mut out = BTreeMap::new();
    for u in &ir.unions {
        for id in u.entries.iter().flat_map(|e| &e.ids) {
            if id.kind == "const" {
                out.insert(id.name.as_str(), u.interface.as_str());
            }
        }
    }
    out
}

/// The value kind of a group, failing on a mix.
fn basic(g: &Group<'_>) -> Result<Basic, String> {
    let mut kinds = g.members.iter().map(|m| match m.c.basic.as_str() {
        "untyped string" | "string" => Ok(Basic::Str),
        "untyped float" | "float32" | "float64" => Ok(Basic::Float),
        "untyped int" | "int" | "uint" | "uint8" | "int8" | "uint16" | "int16" | "uint32"
        | "int32" | "uint64" | "int64" => Ok(Basic::Int),
        other => Err(format!("{}: unsupported basic type {other}", m.c.name)),
    });
    let first = kinds.next().ok_or("empty group")??;
    for k in kinds {
        if k? != first {
            return Err("constants of different kinds".to_owned());
        }
    }
    Ok(first)
}

/// The integer type of a group: the Go type of typed constants, else the table's or the kind's
/// default. Go `int` and `uint` have no fixed size and count as untyped.
fn int_type(g: &Group<'_>) -> Result<IntType, String> {
    let untyped = g
        .members
        .iter()
        .filter(|m| m.c.ty.starts_with("untyped") || matches!(m.c.basic.as_str(), "int" | "uint"))
        .count();
    let basics: BTreeSet<&str> = g.members.iter().map(|m| m.c.basic.as_str()).collect();
    if untyped == g.members.len() {
        IntType::new(match (g.spec.repr, g.spec.kind) {
            ("", Mask | Index) => "u32",
            ("", _) => "i32",
            (repr, _) => repr,
        })
    } else if untyped == 0 && basics.len() == 1 {
        if !g.spec.repr.is_empty() {
            return Err("repr given for typed constants".to_owned());
        }
        IntType::of_go(basics.first().copied().unwrap_or_default())
    } else {
        Err(format!("mixed constant types {basics:?}"))
    }
}

/// A member resolved for emission.
#[derive(Debug)]
struct Resolved<'a> {
    m: &'a Member<'a>,
    role: Role,
    /// The integer value on the wire (ints only).
    value: i128,
}

/// What a member becomes.
#[derive(Debug)]
enum Role {
    Variant(String),
    /// An associated constant naming the variant it is defined as.
    Alias {
        name: String,
        of: String,
    },
    /// A count marker.
    Sentinel(String),
}

/// Emits one group.
fn group_items(
    g: &Group<'_>,
    name: &str,
    groups: &[Group<'_>],
    tags: &BTreeMap<&str, &str>,
    out: &mut Output,
) -> Result<(), String> {
    match g.spec.kind {
        UnionTag => {
            let union = g
                .members
                .iter()
                .find_map(|m| tags.get(m.c.name.as_str()))
                .ok_or("no constant of the group is a union tag")?;
            exclude(g, &format!("tags of the generated union {union}"), out);
            Ok(())
        }
        Excluded(reason) => {
            exclude(g, reason, out);
            Ok(())
        }
        SameAs(other) => {
            let o = find(groups, other)?;
            let pairs = |g: &Group<'_>| -> Vec<(String, String)> {
                g.members
                    .iter()
                    .map(|m| (m.rest.clone(), m.c.value.clone()))
                    .collect()
            };
            if pairs(g) != pairs(o) {
                return Err(format!("differs from {other}"));
            }
            exclude(g, &format!("the same as {other}"), out);
            Ok(())
        }
        BitsOf(other) => {
            let o = find(groups, other)?;
            for m in &g.members {
                if g.spec.sentinels.contains(&m.rest.as_str()) {
                    continue;
                }
                let mask = o
                    .members
                    .iter()
                    .find(|x| x.rest == m.rest)
                    .ok_or_else(|| format!("{}: not in {other}", m.c.name))?;
                let bit: u32 =
                    m.c.value
                        .parse()
                        .map_err(|e| format!("{}: {e}", m.c.name))?;
                let mask: u64 = mask
                    .c
                    .value
                    .parse()
                    .map_err(|e| format!("{}: {e}", m.c.name))?;
                if 1_u64.checked_shl(bit) != Some(mask) {
                    return Err(format!("{}: {bit} is not the bit of {other}", m.c.name));
                }
            }
            exclude(g, &format!("the bit positions of {other} (Flag::bit)"), out);
            Ok(())
        }
        Enum | Mask | Index | Values => match basic(g)? {
            Basic::Str => str_enum(g, name, out),
            Basic::Float => values(g, None, out),
            Basic::Int if matches!(g.spec.kind, Values) => values(g, Some(int_type(g)?), out),
            Basic::Int => int_enum(g, name, out),
        },
    }
}

fn find<'g, 'a>(groups: &'g [Group<'a>], key: &str) -> Result<&'g Group<'a>, String> {
    groups
        .iter()
        .find(|g| g.spec.key == key)
        .ok_or_else(|| format!("no group {key}"))
}

/// Lists the constants of a group as left out, with the reason as a comment.
fn exclude(g: &Group<'_>, reason: &str, out: &mut Output) {
    put!(
        out.excluded,
        "        // {}: {reason}
",
        g.spec.key
    );
    for m in &g.members {
        put!(
            out.excluded,
            "        {},
",
            str_lit(&m.c.name)
        );
    }
}

/// The `clippy::enum_variant_names` expectation for an enum whose variants (`Self::X`, at
/// least three) all start or all end with the same word, which gophertunnel's names do.
fn variant_names_expectation(all: &[String]) -> &'static str {
    let words: Vec<Vec<String>> = all
        .iter()
        .map(|v| names::words(v.strip_prefix("Self::").unwrap_or(v)))
        .collect();
    let shared = |pick: fn(&Vec<String>) -> Option<&String>| {
        let first = words.first().and_then(pick);
        first.is_some() && words.iter().all(|w| w.len() > 1 && pick(w) == first)
    };
    if words.len() >= 3 && (shared(|w| w.first()) || shared(|w| w.last())) {
        "#[expect(clippy::enum_variant_names, reason = \"consistency: gophertunnel's constant names\")]\n"
    } else {
        ""
    }
}

/// A Rust integer literal, grouped as `clippy::unreadable_literal` wants.
fn lit(value: i128) -> String {
    int_lit(&value.to_string())
}

/// The lowercase words of a Rust type name, for error messages (`container type`).
fn label(name: &str) -> String {
    names::words(name)
        .iter()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The doc of a group.
fn group_doc(g: &Group<'_>) -> String {
    let (package, name) = g.spec.key.split_once('.').unwrap_or_default();
    let own = [name];
    let prefixes: &[&str] = if g.spec.prefixes.is_empty() {
        &own
    } else {
        g.spec.prefixes
    };
    let names: Vec<String> = prefixes
        .iter()
        .map(|p| format!("`{package}.{p}*`"))
        .collect();
    doc(
        &format!("The gophertunnel {} constants.", names.join(", ")),
        "",
    )
}

/// The doc of a member: its Go name.
fn member_doc(m: &Member<'_>, extra: &str, indent: &str) -> String {
    doc(&format!("gophertunnel `{}`{extra}.", m.go), indent)
}

/// Resolves the members of an integer group: variants, aliases and sentinels, with checks.
fn resolve<'a>(g: &'a Group<'a>, ty: IntType) -> Result<Vec<Resolved<'a>>, String> {
    let mut out = Vec::with_capacity(g.members.len());
    let mut variants: BTreeMap<String, String> = BTreeMap::new();
    let mut values: BTreeSet<i128> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::from(["ALL".to_owned()]);
    for m in &g.members {
        let value: i128 =
            m.c.value
                .parse()
                .map_err(|e| format!("{}: {e}", m.c.name))?;
        let role = if g.spec.sentinels.contains(&m.rest.as_str()) {
            Role::Sentinel(names::screaming(&m.rest, &m.c.name)?)
        } else if let Some(alias) = &m.c.alias {
            let of = variants
                .get(alias)
                .ok_or_else(|| format!("{}: alias of {alias}, not an earlier member", m.c.name))?
                .clone();
            Role::Alias {
                name: names::screaming(&m.rest, &m.c.name)?,
                of,
            }
        } else {
            let variant = names::camel(&m.rest, &m.c.name)?;
            variants.insert(m.c.name.clone(), variant.clone());
            Role::Variant(variant)
        };
        let ident = match &role {
            Role::Variant(n) | Role::Alias { name: n, .. } | Role::Sentinel(n) => n.clone(),
        };
        if !seen.insert(ident.clone()) {
            return Err(format!("{}: name `{ident}` is taken", m.c.name));
        }
        let wire = if matches!(role, Role::Sentinel(_)) {
            value
        } else {
            ty.wire(value).map_err(|e| format!("{}: {e}", m.c.name))?
        };
        if matches!(role, Role::Variant(_)) && !values.insert(wire) {
            return Err(format!(
                "{}: value {value} repeats without an alias",
                m.c.name
            ));
        }
        out.push(Resolved {
            m,
            role,
            value: wire,
        });
    }
    Ok(out)
}

/// The bit of a flag value, checking the kind's rule. Whether the bits fit a `FlagSet` is a
/// compile-time check in `src/protocol/flags.rs`, which the generated tests reach for every
/// flag group.
fn flag_bit(kind: Kind, value: i128) -> Result<u32, String> {
    match kind {
        Mask if value > 0 && value.count_ones() == 1 => Ok(value.trailing_zeros()),
        Index => u32::try_from(value).map_err(|e| format!("{value} is not a bit position: {e}")),
        _ => Err(format!("{value} is not a flag of a {kind:?} group")),
    }
}

/// An integer enum (and its `Flag` implementation for flag groups).
fn int_enum(g: &Group<'_>, name: &str, out: &mut Output) -> Result<(), String> {
    let ty = int_type(g)?;
    let resolved = resolve(g, ty)?;
    let label = str_lit(&label(name));
    let r = ty.name;
    let flags = matches!(g.spec.kind, Mask | Index);
    let (mut variants, mut arms, mut bits, mut from_bits) =
        (String::new(), String::new(), String::new(), String::new());
    let mut all = Vec::new();
    let mut go_names = Vec::new();
    let mut members: BTreeSet<i128> = BTreeSet::new();
    let mut count = 0;
    for x in &resolved {
        let Role::Variant(v) = &x.role else {
            continue;
        };
        members.insert(x.value);
        let go: i128 = x.m.c.value.parse().unwrap_or(x.value);
        let extra = if go == x.value {
            String::new()
        } else {
            format!(" ({go}, sent as {})", x.value)
        };
        variants.push_str(&member_doc(x.m, &extra, "    "));
        put!(variants, "    {v} = {},\n", lit(x.value));
        put!(arms, "            {} => Ok(Self::{v}),\n", lit(x.value));
        if flags {
            let bit = flag_bit(g.spec.kind, x.value).map_err(|e| format!("{}: {e}", x.m.c.name))?;
            count = count.max(bit + 1);
            put!(bits, "            Self::{v} => {bit},\n");
            put!(from_bits, "            {bit} => Some(Self::{v}),\n");
        }
        all.push(format!("Self::{v}"));
        go_names.push(str_lit(&x.m.c.name));
    }
    put!(
        out.values,
        "ints(&mut v, &[{}], &{name}::ALL.map(|x| i128::from({r}::from(x))), {});\n",
        go_names.join(", "),
        ty.unsigned_bits()
    );
    let assoc = associated(g, name, ty, &resolved, &members, count, out)?;
    out.items.push_str(&group_doc(g));
    put!(
        out.items,
        "{}#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]\n#[repr({r})]\npub enum {name} {{\n{variants}}}\n\n",
        variant_names_expectation(&all)
    );
    put!(
        out.items,
        "impl {name} {{\n    /// Every value, in gophertunnel's order.\n    pub const ALL: [Self; {}] = [{}];\n{assoc}}}\n\n",
        all.len(),
        all.join(", ")
    );
    let unknown = if ty.bits == 64 && !ty.signed {
        "i64::try_from(value).unwrap_or(i64::MAX)"
    } else {
        "value.into()"
    };
    put!(
        out.items,
        "impl TryFrom<{r}> for {name} {{\n    type Error = Error;\n\n    fn try_from(value: {r}) -> Result<Self> {{\n        match value {{\n{arms}            _ => Err(Error::UnknownEnum {{\n                name: {label},\n                value: {unknown},\n            }}),\n        }}\n    }}\n}}\n\n\
         impl From<{name}> for {r} {{\n    fn from(value: {name}) -> Self {{\n        value as Self\n    }}\n}}\n\n"
    );
    if flags {
        if count == 0 {
            return Err("no flags".to_owned());
        }
        put!(
            out.items,
            "impl Flag for {name} {{\n    const COUNT: u32 = {count};\n    const NAME: &'static str = {label};\n\n    fn bit(self) -> u32 {{\n        match self {{\n{bits}        }}\n    }}\n\n    fn from_bit(bit: u32) -> Option<Self> {{\n        match bit {{\n{from_bits}            _ => None,\n        }}\n    }}\n}}\n\n"
        );
        out.checks.push(format!("flag(&{name}::ALL);"));
    }
    let unknown = unknown_value(ty, &members).map_or_else(
        || format!("None::<{r}>"),
        |v| format!("Some({}_{r})", lit(v)),
    );
    out.checks
        .push(format!("int_enum(&{name}::ALL, {unknown});"));
    Ok(())
}

/// The associated constants of an integer enum: aliases and count markers (checked; the count of
/// a flag group is its `Flag::COUNT`).
fn associated(
    g: &Group<'_>,
    name: &str,
    ty: IntType,
    resolved: &[Resolved<'_>],
    members: &BTreeSet<i128>,
    count: u32,
    out: &mut Output,
) -> Result<String, String> {
    let r = ty.name;
    let wire_bits = ty.unsigned_bits();
    let mut assoc = String::new();
    for x in resolved {
        let go = str_lit(&x.m.c.name);
        match &x.role {
            Role::Variant(_) => {}
            Role::Alias { name: alias, of } => {
                assoc.push_str(&member_doc(x.m, "", "    "));
                put!(assoc, "    pub const {alias}: Self = Self::{of};\n");
                put!(
                    out.values,
                    "v.push(({go}, Value::Int(i128::from({r}::from({name}::{alias})), {wire_bits})));\n"
                );
            }
            Role::Sentinel(sentinel) => {
                let next = match g.spec.kind {
                    Mask => 1_i128 << count,
                    _ => members.last().map_or(0, |m| m + 1),
                };
                if x.value != next {
                    return Err(format!("{go}: count marker {} is not {next}", x.value));
                }
                match g.spec.kind {
                    Mask => put!(
                        out.values,
                        "v.push(({go}, Value::Int(1_i128 << <{name} as Flag>::COUNT, 0)));\n"
                    ),
                    Index => put!(
                        out.values,
                        "v.push(({go}, Value::Int(i128::from(<{name} as Flag>::COUNT), 0)));\n"
                    ),
                    _ => {
                        let value = ty.wire(x.value)?;
                        assoc.push_str(&member_doc(x.m, ": one past the last value", "    "));
                        put!(assoc, "    pub const {sentinel}: {r} = {};\n", lit(value));
                        put!(
                            out.values,
                            "v.push(({go}, Value::Int(i128::from({name}::{sentinel}), {wire_bits})));\n"
                        );
                    }
                }
            }
        }
    }
    Ok(assoc)
}

/// A value of `ty` outside `members` for the unknown-value test: past either end, else the
/// first gap (at most one step per member); `None` when the set fills the type.
fn unknown_value(ty: IntType, members: &BTreeSet<i128>) -> Option<i128> {
    let span = 1_i128 << ty.bits;
    let (lo, hi) = if ty.signed {
        (-(span / 2), span / 2 - 1)
    } else {
        (0, span - 1)
    };
    let gaps = (0..=members.len()).filter_map(|i| i128::try_from(i).ok().map(|i| lo + i));
    [
        members.last().map(|m| m + 1),
        members.first().map(|m| m - 1),
    ]
    .into_iter()
    .flatten()
    .chain(gaps)
    .find(|v| (lo..=hi).contains(v) && !members.contains(v))
}

/// A string enum.
fn str_enum(g: &Group<'_>, name: &str, out: &mut Output) -> Result<(), String> {
    if !g.spec.sentinels.is_empty() || !matches!(g.spec.kind, Enum) {
        return Err("string groups are plain enums".to_owned());
    }
    let label = label(name);
    let mut variants = String::new();
    let mut all = Vec::new();
    let mut arms = String::new();
    let mut sorted: BTreeMap<&str, String> = BTreeMap::new();
    let mut seen = BTreeSet::from(["ALL".to_owned()]);
    let mut go_names = Vec::new();
    for m in &g.members {
        if m.c.alias.is_some() {
            return Err(format!("{}: string alias", m.c.name));
        }
        let v = names::camel(&m.rest, &m.c.name)?;
        if !seen.insert(v.clone()) {
            return Err(format!("{}: name `{v}` is taken", m.c.name));
        }
        if sorted.insert(m.c.value.as_str(), v.clone()).is_some() {
            return Err(format!("{}: string {:?} repeats", m.c.name, m.c.value));
        }
        variants.push_str(&member_doc(
            m,
            &format!(", {}", str_lit(&m.c.value)),
            "    ",
        ));
        put!(variants, "    {v},\n");
        all.push(format!("Self::{v}"));
        put!(arms, "            Self::{v} => {},\n", str_lit(&m.c.value));
        go_names.push(str_lit(&m.c.name));
    }
    put!(
        out.values,
        "strs(&mut v, &[{}], &{name}::ALL.map({name}::as_str));\n",
        go_names.join(", ")
    );
    let by_name: Vec<String> = sorted
        .iter()
        .map(|(s, v)| format!("({}, Self::{v})", str_lit(s)))
        .collect();
    let n = all.len();
    out.items.push_str(&group_doc(g));
    put!(
        out.items,
        "{}#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]\npub enum {name} {{\n{variants}}}\n\n",
        variant_names_expectation(&all)
    );
    put!(
        out.items,
        "impl {name} {{\n    /// Every value, in gophertunnel's order.\n    pub const ALL: [Self; {n}] = [{}];\n    /// The values by string, in byte order for binary search.\n    const BY_STR: [(&'static str, Self); {n}] = [{}];\n\n    /// The string on the wire.\n    pub const fn as_str(self) -> &'static str {{\n        match self {{\n{arms}        }}\n    }}\n}}\n\n",
        all.join(", "),
        by_name.join(", ")
    );
    put!(
        out.items,
        "impl TryFrom<&str> for {name} {{\n    type Error = Error;\n\n    fn try_from(value: &str) -> Result<Self> {{\n        Self::BY_STR\n            .binary_search_by(|&(s, _)| s.cmp(value))\n            .ok()\n            .and_then(|i| Self::BY_STR.get(i))\n            .map(|&(_, v)| v)\n            .ok_or(Error::InvalidValue {{\n                field: {},\n                reason: \"unknown name\",\n            }})\n    }}\n}}\n\n",
        str_lit(&label)
    );
    out.checks
        .push(format!("str_enum(&{name}::ALL, {name}::as_str);"));
    Ok(())
}

/// Plain constants: integers of `ty`, or `f32` floats when `ty` is `None`.
fn values(g: &Group<'_>, ty: Option<IntType>, out: &mut Output) -> Result<(), String> {
    if !g.spec.sentinels.is_empty() || !matches!(g.spec.kind, Values) {
        return Err("float constants and open sets are Values groups".to_owned());
    }
    put!(
        out.items,
        "// The gophertunnel `{}` constants.\n\n",
        g.spec.key
    );
    for m in &g.members {
        if m.c.alias.is_some() {
            return Err(format!("{}: alias in a Values group", m.c.name));
        }
        let name = names::screaming(m.go, &m.c.name)?;
        let go = str_lit(&m.c.name);
        out.items.push_str(&member_doc(m, "", ""));
        if let Some(ty) = ty {
            let value: i128 = m.c.value.parse().map_err(|e| format!("{go}: {e}"))?;
            let value = ty.wire(value)?;
            put!(
                out.items,
                "pub const {name}: {} = {};\n",
                ty.name,
                lit(value)
            );
            put!(
                out.values,
                "v.push(({go}, Value::Int(i128::from({name}), {})));\n",
                ty.unsigned_bits()
            );
        } else {
            let value: f32 = m.c.value.parse().map_err(|e| format!("{go}: {e}"))?;
            if !value.is_finite() {
                return Err(format!("{go}: not a finite f32"));
            }
            let mut lit = m.c.value.clone();
            if !lit.contains(['.', 'e', 'E']) {
                lit.push_str(".0");
            }
            put!(out.items, "pub const {name}: f32 = {lit};\n");
            put!(out.values, "v.push(({go}, Value::Float({name})));\n");
        }
    }
    out.items.push('\n');
    Ok(())
}

/// The test module: every Rust value against the IR, round trips and unknown values. The
/// placeholders `@VALUES@`, `@EXCLUDED@` and `@CHECKS@` take the generated lines.
const TESTS: &str = r#"#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Debug;

    use super::*;

    /// A constant's value as the Rust item gives it. Integers carry the bit width of their
    /// unsigned Rust type (0 when signed): negative Go values wrap to it, as gophertunnel writes
    /// them.
    #[derive(Debug)]
    enum Value {
        Int(i128, u32),
        Float(f32),
        Str(&'static str),
    }

    /// The Rust value of every generated constant, by Go name.
    fn values() -> Vec<(&'static str, Value)> {
        let mut v = Vec::new();
        @VALUES@
        v
    }

    fn ints(out: &mut Vec<(&'static str, Value)>, names: &[&'static str], values: &[i128], bits: u32) {
        assert_eq!(names.len(), values.len());
        out.extend(names.iter().zip(values).map(|(&n, &x)| (n, Value::Int(x, bits))));
    }

    fn strs(out: &mut Vec<(&'static str, Value)>, names: &[&'static str], values: &[&'static str]) {
        assert_eq!(names.len(), values.len());
        out.extend(names.iter().zip(values).map(|(&n, &x)| (n, Value::Str(x))));
    }

    /// The Go constants left out (their groups are in `xtask/src/gen/consts.rs`).
    const EXCLUDED: &[&str] = &[
@EXCLUDED@    ];

    #[test]
    fn every_gophertunnel_constant_has_its_value_or_is_excluded() {
        let ir: serde_json::Value =
            serde_json::from_str(include_str!("../../xtask/data/protocol.json")).unwrap();
        let rust: BTreeMap<&str, Value> = values().into_iter().collect();
        let excluded: BTreeSet<&str> = EXCLUDED.iter().copied().collect();
        assert_eq!(rust.len(), values().len());
        assert_eq!(excluded.len(), EXCLUDED.len());
        let mut go_names = BTreeSet::new();
        for decl in ir["const_decls"].as_array().unwrap() {
            for c in decl["consts"].as_array().unwrap() {
                let name = c["name"].as_str().unwrap();
                let go = c["value"].as_str().unwrap();
                assert!(go_names.insert(name), "{name} repeats");
                if excluded.contains(name) {
                    assert!(!rust.contains_key(name), "{name} is excluded and generated");
                    continue;
                }
                match rust.get(name) {
                    Some(Value::Int(rust, bits)) => {
                        let go: i128 = go.parse().unwrap();
                        let wire = if *bits > 0 { go.rem_euclid(1 << bits) } else { go };
                        assert_eq!(*rust, wire, "{name}");
                    }
                    Some(Value::Float(rust)) => {
                        assert_eq!(rust.to_bits(), go.parse::<f32>().unwrap().to_bits(), "{name}");
                    }
                    Some(Value::Str(rust)) => assert_eq!(*rust, go, "{name}"),
                    None => panic!("{name} has no Rust item"),
                }
            }
        }
        let covered: BTreeSet<&str> = rust.keys().copied().chain(excluded).collect();
        assert_eq!(covered, go_names);
    }

    fn int_enum<E, R>(all: &[E], unknown: Option<R>)
    where
        E: Copy + PartialEq + Debug + TryFrom<R, Error = Error>,
        R: From<E> + Copy + Debug,
    {
        for &v in all {
            assert_eq!(E::try_from(R::from(v)), Ok(v));
        }
        if let Some(u) = unknown {
            assert!(matches!(E::try_from(u), Err(Error::UnknownEnum { .. })), "{u:?}");
        }
    }

    fn str_enum<E>(all: &[E], as_str: fn(E) -> &'static str)
    where
        E: Copy + PartialEq + Debug + for<'a> TryFrom<&'a str, Error = Error>,
    {
        for &v in all {
            assert_eq!(E::try_from(as_str(v)), Ok(v));
        }
        assert!(E::try_from("\u{0}").is_err());
    }

    fn flag<F: Flag + PartialEq + Debug>(all: &[F]) {
        let mut bits = BTreeSet::new();
        for &v in all {
            assert!(v.bit() < F::COUNT);
            assert!(bits.insert(v.bit()));
            assert_eq!(F::from_bit(v.bit()), Some(v));
        }
        assert_eq!(F::from_bit(F::COUNT), None);
        let set: super::super::FlagSet<F> = all.iter().copied().collect();
        assert_eq!(set.iter().collect::<Vec<_>>().len(), all.len());
    }

    #[test]
    fn every_enum_round_trips_and_rejects_unknown_values() {
        @CHECKS@
    }
}
"#;

/// Appends the test module.
fn tests(text: &mut String, out: &Output) {
    text.push_str(
        &TESTS
            .replace("@VALUES@", &out.values)
            .replace("@EXCLUDED@", &out.excluded)
            .replace("@CHECKS@", &out.checks.join("\n")),
    );
}
