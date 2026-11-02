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
use super::{model, names};
use Kind::{BitsOf, Enum, Excluded, Index, Mask, SameAs, UnionTag, Values};

/// The generated file, relative to the repository root.
pub(crate) const PATH: &str = "src/protocol/consts.rs";

/// Every constant group, in the order of the generated file. `repr` follows the packet field
/// that carries the group (the one of the current packet when several do).
const SPECS: &[Spec] = &[
    // protocol
    Spec::new("protocol.Ability", Mask).with_repr("u32").count(),
    Spec::new("protocol.AbilityLayerType", Enum).with_repr("u16"),
    Spec::new("protocol.AbilityBase", Values),
    Spec::new("protocol.AttributeModifierOperation", Enum),
    Spec::new("protocol.AttributeModifierOperand", Enum),
    Spec::new("protocol.AttributeLayerPayloadType", Enum).with_repr("u32"),
    Spec::new("protocol.EnvironmentAttributePayloadType", Enum).with_repr("u32"),
    Spec::new("protocol.AttributeDataType", Enum).with_repr("u32"),
    Spec::new("protocol.AttributeBoolOperation", Enum),
    Spec::new("protocol.AttributeFloatOperation", Enum),
    Spec::new("protocol.AttributeColourOperation", Enum),
    Spec::new("protocol.NoiseAlignmentType", Enum).with_repr("u8"),
    Spec::new("protocol.BiomeExpressionOp", Enum)
        .with_prefixes(&["BiomeExpressionOp", "BiomeExpression"])
        .count(),
    Spec::new("protocol.BiomeCoordinateEvaluationOrder", Enum),
    Spec::new("protocol.BiomeRandomDistributionType", Enum),
    Spec::new("protocol.AimAssistTargetMode", Enum),
    Spec::new("protocol.AudioListener", Enum).with_repr("u8"),
    Spec::new("protocol.AudioContentPlaybackType", Enum),
    Spec::new("protocol.EasingType", Enum),
    Spec::new("protocol.SplineType", Enum),
    Spec::new("protocol.ClockPayloadType", Enum).with_repr("u32"),
    Spec::new("protocol.CommandPermissionLevel", Enum).with_repr("u8"),
    Spec::new("protocol.CommandArg", Mask).with_repr("u32"),
    Spec::new("protocol.CommandArgType", Enum).with_repr("u32"),
    Spec::new("protocol.ParamOption", Mask).with_repr("u8"),
    Spec::new("protocol.CommandEnumConstraint", Enum).with_repr("u8"),
    Spec::new("protocol.CommandOrigin", Enum).with_repr("u32"),
    Spec::new("protocol.Container", Enum).with_repr("u8"),
    Spec::new("protocol.ContainerType", Enum).with_repr("u8"),
    Spec::new("protocol.CreativeCategory", Enum).with_repr("u8"),
    Spec::new("protocol.DataStoreChangeType", Enum).with_repr("u32"),
    Spec::new("protocol.DataStoreControl", Enum).with_repr("u32"),
    Spec::new("protocol.DataStorePropertyType", Enum),
    Spec::new("protocol.EntityLink", Enum).with_repr("u8"),
    Spec::new("protocol.PassengerOfBlockEmote", Enum).with_repr("u8"),
    Spec::new("protocol.EntityDataKey", Enum).with_repr("u32"),
    Spec::new("protocol.EntityDataFlag", Index)
        .with_repr("u32")
        .count(),
    Spec::new("protocol.EntityDataType", Enum),
    Spec::new("protocol.EventType", UnionTag),
    Spec::new("protocol.CopperBlockID", Enum)
        .with_prefixes(&["Wax", "UnWax"])
        .whole(),
    Spec::new("protocol.FurnaceLayout", Enum),
    Spec::new("protocol.FurnaceLeftTab", Enum),
    Spec::new("protocol.GraphicsOverrideParameterType", Enum),
    Spec::new(
        "protocol.Current",
        Excluded("packet::PROTOCOL_VERSION and packet::GAME_VERSION"),
    ),
    Spec::new("protocol.InventoryActionSource", Enum).with_repr("u32"),
    Spec::new("protocol.WindowID", Values).with_repr("u32"),
    Spec::new("protocol.InventoryTransactionType", UnionTag),
    Spec::new("protocol.UseItemAction", Enum).with_repr("u32"),
    Spec::new("protocol.TriggerType", Enum).with_repr("u32"),
    Spec::new("protocol.ClientPrediction", Enum).with_repr("u8"),
    Spec::new("protocol.ClientCooldownState", Enum).with_repr("u8"),
    Spec::new("protocol.HandSlot", Enum).with_repr("u8"),
    Spec::new("protocol.UseItemOnEntityAction", Enum),
    Spec::new("protocol.ReleaseItemAction", Enum),
    Spec::new("protocol.ItemEntryVersion", Enum),
    Spec::new("protocol.ItemDescriptor", UnionTag),
    Spec::new("protocol.FilterCause", Enum),
    Spec::new("protocol.ItemStackResponseStatus", Enum).with_repr("u8"),
    Spec::new("protocol.StackRequestAction", UnionTag),
    Spec::new("protocol.MapDecorationType", Enum).with_repr("u8"),
    Spec::new("protocol.MapObjectType", Enum),
    Spec::new("protocol.MemoryCategory", Enum).with_repr("u8"),
    Spec::new("protocol.DeviceOS", Enum).with_prefixes(&["Device"]),
    Spec::new("protocol.PlayerListAction", Enum).with_repr("u8"),
    Spec::new("protocol.PlayerAction", Enum).count(),
    Spec::new("protocol.RecipeUnlockContext", Enum),
    Spec::new("protocol.PackSettingType", Enum).with_repr("u32"),
    Spec::new("protocol.ScoreboardIdentity", Enum).with_repr("u8"),
    Spec::new("protocol.ShapeData", UnionTag),
    Spec::new("protocol.PrimitiveShape", Enum),
    Spec::new("protocol.ArmSize", Enum).with_repr("u8"),
    Spec::new("protocol.SkinAnimation", Enum).with_repr("u32"),
    Spec::new("protocol.ExpressionType", Enum).with_repr("u32"),
    Spec::new("protocol.PieceType", Enum).with_repr("u32"),
    Spec::new("protocol.SoundDataUpdate", Enum),
    Spec::new("protocol.StructureMirror", Enum).with_repr("u8"),
    Spec::new("protocol.StructureRotation", Enum).with_repr("u8"),
    Spec::new("protocol.AnimationMode", Enum).with_repr("u8"),
    Spec::new("protocol.HeightMapData", Enum).with_repr("u8"),
    Spec::new("protocol.SubChunkResult", Enum).with_repr("u8"),
    Spec::new("protocol.WaypointAction", Enum).with_repr("u8"),
    Spec::new("protocol.WaypointUpdateFlag", Mask).with_repr("u32"),
    Spec::new("protocol.WaypointTexture", Enum),
    Spec::new("protocol.Generator", Enum),
    // packet
    Spec::new("packet.ActorEvent", Enum).with_repr("u8"),
    Spec::new("packet.AdventureFlag", Mask)
        .with_repr("u32")
        .with_prefixes(&["AdventureFlag", "AdventureSettingsFlags"]),
    Spec::new("packet.ActionPermission", Mask).with_repr("u32"),
    Spec::new("packet.PermissionLevel", Enum).with_repr("u8"),
    Spec::new("packet.AgentActionType", Enum),
    Spec::new("packet.AnimateAction", Enum).with_repr("u8"),
    Spec::new("packet.AnimateSwingSource", Enum).with_repr("u8"),
    Spec::new("packet.BlockEventType", Enum).with_prefixes(&["BlockEvent"]),
    Spec::new("packet.BookAction", Enum).with_repr("u32"),
    Spec::new("packet.BossEventType", Enum)
        .with_repr("u8")
        .with_prefixes(&["BossEvent"]),
    Spec::new("packet.BossEventColour", Enum).with_repr("u8"),
    Spec::new("packet.BossEventOverlay", Enum).with_repr("u8"),
    Spec::new("packet.CameraAimAssistAction", Enum).with_repr("u8"),
    Spec::new("packet.CameraAunAssistPresetOperation", Enum).with_repr("u8"),
    Spec::new("packet.CameraShakeType", Enum),
    Spec::new("packet.CameraShakeAction", Enum).with_repr("u8"),
    Spec::new("packet.Dimension", Enum),
    Spec::new("packet.ControlScheme", Enum).with_repr("u8"),
    Spec::new("packet.ClientBoundDebugRendererType", Enum)
        .with_repr("u32")
        .with_prefixes(&["ClientBoundDebugRenderer"]),
    Spec::new("packet.ClientCameraAimAssistAction", Enum).with_repr("u8"),
    Spec::new("packet.TextureShiftAction", Enum).with_repr("u8"),
    Spec::new("packet.CodeBuilderOperation", Enum).with_repr("u8"),
    Spec::new("packet.CodeBuilderCategory", Enum).with_repr("u8"),
    Spec::new("packet.CodeBuilderStatus", Enum).with_repr("u8"),
    Spec::new("packet.CommandBlockMode", Enum)
        .with_repr("u32")
        .with_prefixes(&["CommandBlock"]),
    Spec::new("packet.CommandOutputType", Enum).with_repr("u8"),
    Spec::new("packet.UseItemMethod", Enum).with_prefixes(&["UseItem"]),
    Spec::new("packet.CursorItemDrag", Enum).with_repr("u8"),
    Spec::new("packet.ContainerDataFurnace", Enum),
    Spec::new("packet.ContainerDataBrewingStand", Enum),
    Spec::new("packet.PredictionType", Enum).with_repr("u8"),
    Spec::new(
        "packet.DefaultMaxDecompressedLen",
        Excluded("the max-batch-bytes setting"),
    ),
    Spec::new("packet.DisconnectReason", Enum),
    Spec::new("packet.EmoteFlag", Mask).with_repr("u8"),
    Spec::new("packet.GameTestRequestRotation", Enum)
        .with_repr("u8")
        .with_prefixes(&["GameTestRequest"]),
    Spec::new("packet.ID", Excluded("packet IDs: the Packet enum")),
    Spec::new("packet.InteractAction", Enum).with_repr("u8"),
    Spec::new(
        "packet.InventoryTransactionType",
        SameAs("protocol.InventoryTransactionType"),
    ),
    Spec::new("packet.LabTableAction", Enum).with_repr("u8"),
    Spec::new("packet.LessonAction", Enum),
    Spec::new("packet.MatchmakingState", Enum).with_repr("u8"),
    Spec::new("packet.ParticleType", Enum),
    Spec::new("packet.LevelEvent", Enum),
    Spec::new("packet.SoundEvent", Enum),
    Spec::new("packet.MobEffectOperation", Enum)
        .with_repr("u8")
        .with_prefixes(&["MobEffect"]),
    Spec::new("packet.Effect", Enum),
    Spec::new("packet.ModalFormCancelReason", Enum).with_repr("u8"),
    Spec::new("packet.MoveFlag", Mask).with_repr("u8"),
    Spec::new("packet.MoveMode", Enum).with_repr("u8"),
    Spec::new("packet.TeleportCause", Enum),
    Spec::new("packet.MovementEffectType", Enum),
    Spec::new("packet.MultiPlayerSettingsAction", Enum)
        .with_prefixes(&["EnableMultiPlayer", "DisableMultiPlayer", "RefreshJoinCode"])
        .whole(),
    Spec::new("packet.CompressionAlgorithm", Enum).with_repr("u16"),
    Spec::new("packet.NPCDialogueAction", Enum),
    Spec::new("packet.NPCRequestAction", Enum).with_repr("u8"),
    Spec::new("packet.ViolationType", Enum),
    Spec::new("packet.ViolationSeverity", Enum),
    Spec::new("packet.PhotoType", Enum),
    Spec::new("packet.PlayStatus", Enum),
    Spec::new("packet.PlayerArmourDamageFlag", Index).with_repr("u8"),
    Spec::new("packet.InputFlag", Index)
        .with_repr("u32")
        .count(),
    Spec::new("packet.InputMode", Enum).with_repr("u32").count(),
    Spec::new("packet.PlayMode", Enum)
        .with_repr("u32")
        .with_sentinels(&["NumModes"]),
    Spec::new("packet.InteractionModel", Enum).count(),
    Spec::new("packet.PlayerLocationType", Enum),
    Spec::new("packet.PlayerUpdateEntityOverridesType", Enum).with_repr("u32"),
    Spec::new("packet.PlayerVideoCaptureAction", Enum).with_repr("u8"),
    Spec::new("packet.PositionTrackingDBRequestAction", Enum).with_repr("u8"),
    Spec::new("packet.PositionTrackingDBBroadcastAction", Enum).with_repr("u8"),
    Spec::new("packet.Ability", BitsOf("protocol.Ability")).count(),
    Spec::new("packet.PackResponse", Enum).with_repr("u32"),
    Spec::new("packet.ResourcePackType", Enum).with_repr("u8"),
    Spec::new("packet.RespawnState", Enum).with_repr("u8"),
    Spec::new("packet.PartyDestinationCookieIntent", Enum),
    Spec::new("packet.LoadingScreenType", Enum),
    Spec::new("packet.DataDrivenScreenCloseReason", Enum),
    Spec::new("packet.ScoreboardSortOrder", Enum),
    Spec::new("packet.ScoreboardSlot", Enum),
    Spec::new("packet.HudElement", Enum),
    Spec::new("packet.HudVisibility", Enum),
    Spec::new("packet.FurnaceType", Enum).with_repr("u8"),
    Spec::new("packet.GameType", Enum),
    Spec::new("packet.InventoryLayout", Enum),
    Spec::new("packet.InventoryLeftTab", Enum),
    Spec::new("packet.InventoryRightTab", Enum),
    Spec::new("packet.ScoreboardIdentityAction", Enum).with_repr("u8"),
    Spec::new("packet.SpawnType", Enum),
    Spec::new("packet.TitleAction", Enum),
    Spec::new("packet.ShowCreditsStatus", Enum),
    Spec::new("packet.StoreOfferType", Enum).with_repr("u8"),
    Spec::new("packet.SimpleEvent", Enum).with_repr("u16"),
    Spec::new("packet.SimulationType", Enum),
    Spec::new("packet.SpawnBiomeType", Enum).with_repr("i16"),
    Spec::new("packet.XBLBroadcastMode", Enum),
    Spec::new("packet.ChatRestrictionLevel", Enum).with_repr("u8"),
    Spec::new("packet.EditorWorldType", Enum),
    Spec::new("packet.EditorLevelMigrationVersion", Enum).with_repr("i8"),
    Spec::new("packet.StructureBlockType", Enum).with_prefixes(&["StructureBlock"]),
    Spec::new("packet.StructureRedstoneSaveMode", Enum).with_repr("u8"),
    Spec::new("packet.StructureTemplateRequestType", Enum)
        .with_repr("u8")
        .with_prefixes(&["StructureTemplateRequest"]),
    Spec::new("packet.StructureTemplateResponseType", Enum)
        .with_repr("u8")
        .with_prefixes(&["StructureTemplateResponse"]),
    Spec::new("packet.TextType", Enum).with_repr("u8"),
    Spec::new("packet.TextCategory", Enum),
    Spec::new("packet.UnlockedRecipesType", Enum).with_repr("u32"),
    Spec::new("packet.BlockUpdateFlag", Mask)
        .with_repr("u32")
        .with_prefixes(&["BlockUpdate"]),
    Spec::new("packet.BlockSyncedTransition", Enum)
        .with_repr("u64")
        .with_prefixes(&["BlockToEntityTransition", "EntityToBlockTransition"])
        .whole(),
    Spec::new("packet.ClientInputLock", Mask).with_repr("u32"),
    Spec::new("packet.GraphicsMode", Enum).with_repr("u8"),
    Spec::new("packet.SoftEnumAction", Enum).with_repr("u8"),
];

/// The test module: every Rust value against the IR, round trips and unknown values. The
/// placeholders `@VALUES@`, `@EXCLUDED@` and `@CHECKS@` take the generated lines.
const TESTS: &str = r#"#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Debug;

    use crate::protocol::FlagSet;

    /// The Go constants left out (their groups are in `xtask/src/gen/consts.rs`).
    const EXCLUDED: &[&str] = &[
@EXCLUDED@    ];

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
        let set: FlagSet<F> = all.iter().copied().collect();
        assert_eq!(set.iter().collect::<Vec<_>>().len(), all.len());
    }

    #[test]
    fn every_enum_round_trips_and_rejects_unknown_values() {
        @CHECKS@
    }
}
"#;

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

impl Spec {
    const fn new(key: &'static str, kind: Kind) -> Self {
        Self {
            key,
            kind,
            repr: "",
            prefixes: &[],
            sentinels: &[],
            whole: false,
        }
    }

    const fn count(self) -> Self {
        Self {
            sentinels: &["Count"],
            ..self
        }
    }

    const fn whole(self) -> Self {
        Self {
            whole: true,
            ..self
        }
    }

    const fn with_prefixes(self, prefixes: &'static [&'static str]) -> Self {
        Self { prefixes, ..self }
    }

    const fn with_repr(self, repr: &'static str) -> Self {
        Self { repr, ..self }
    }

    const fn with_sentinels(self, sentinels: &'static [&'static str]) -> Self {
        Self { sentinels, ..self }
    }
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
        Self::new(model::basic(basic)?)
    }

    /// The bit width the test wraps Go values to: the width when unsigned, else 0.
    fn unsigned_bits(self) -> u32 {
        if self.signed { 0 } else { self.bits }
    }

    /// The wire value of a Go value: negative values of unsigned types wrap, as gophertunnel
    /// writes them (`byte(ContainerTypeInventory)` is 255). Fails if it does not fit.
    fn wire(self, value: i128) -> Result<i128, String> {
        let (min, max) = self.range();
        let value = if !self.signed && value < 0 {
            value + max + 1
        } else {
            value
        };
        if value < min || value > max {
            return Err(format!("{value} does not fit {}", self.name));
        }
        Ok(value)
    }

    /// The smallest and the largest value of the type.
    fn range(self) -> (i128, i128) {
        let span = 1_i128 << self.bits;
        if self.signed {
            (-(span / 2), span / 2 - 1)
        } else {
            (0, span - 1)
        }
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

/// A Rust integer literal of `value`.
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
fn group_doc(g: &Group<'_>) -> Result<String, String> {
    let (package, name) = g
        .spec
        .key
        .split_once('.')
        .ok_or("group key without a package")?;
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
    Ok(doc(
        &format!("The gophertunnel {} constants.", names.join(", ")),
        "",
    ))
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
    let mut seen: BTreeSet<String> = BTreeSet::new();
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
        let go: i128 =
            x.m.c
                .value
                .parse()
                .map_err(|e| format!("{}: {e}", x.m.c.name))?;
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
        "ints(&mut v, &[{}], &{name}::all().iter().map(|&x| i128::from({r}::from(x))).collect::<Vec<_>>(), {});\n",
        go_names.join(", "),
        ty.unsigned_bits()
    );
    let assoc = associated(g, name, ty, &resolved, &members, count, out)?;
    out.items.push_str(&group_doc(g)?);
    put!(
        out.items,
        "{}#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]\n#[repr({r})]\npub enum {name} {{\n{variants}}}\n\n",
        variant_names_expectation(&all)
    );
    put!(
        out.items,
        "impl {name} {{\n    /// Every value, in gophertunnel's order.\n    pub const fn all() -> &'static [Self] {{\n        &[{}]\n    }}\n{assoc}}}\n\n",
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
        out.checks.push(format!("flag({name}::all());"));
    }
    let unknown = unknown_value(ty, &members).map_or_else(
        || format!("None::<{r}>"),
        |v| format!("Some({}_{r})", lit(v)),
    );
    out.checks
        .push(format!("int_enum({name}::all(), {unknown});"));
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
    let (lo, hi) = ty.range();
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
    let mut seen = BTreeSet::new();
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
        "strs(&mut v, &[{}], &{name}::all().iter().map(|&x| x.as_str()).collect::<Vec<_>>());\n",
        go_names.join(", ")
    );
    let by_name: Vec<String> = sorted
        .iter()
        .map(|(s, v)| format!("({}, Self::{v})", str_lit(s)))
        .collect();
    let n = all.len();
    out.items.push_str(&group_doc(g)?);
    put!(
        out.items,
        "{}#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]\npub enum {name} {{\n{variants}}}\n\n",
        variant_names_expectation(&all)
    );
    put!(
        out.items,
        "impl {name} {{\n    /// Every value, in gophertunnel's order.\n    pub const fn all() -> &'static [Self] {{\n        &[{}]\n    }}\n\n    /// The values by string, in byte order for binary search.\n    const BY_STR: [(&'static str, Self); {n}] = [{}];\n\n    /// The string on the wire.\n    pub const fn as_str(self) -> &'static str {{\n        match self {{\n{arms}        }}\n    }}\n}}\n\n",
        all.join(", "),
        by_name.join(", ")
    );
    put!(
        out.items,
        "impl TryFrom<&str> for {name} {{\n    type Error = Error;\n\n    fn try_from(value: &str) -> Result<Self> {{\n        Self::BY_STR\n            .binary_search_by(|&(s, _)| s.cmp(value))\n            .ok()\n            .and_then(|i| Self::BY_STR.get(i))\n            .map(|&(_, v)| v)\n            .ok_or(Error::InvalidValue {{\n                field: {},\n                reason: \"unknown name\",\n            }})\n    }}\n}}\n\n",
        str_lit(&label)
    );
    out.checks
        .push(format!("str_enum({name}::all(), {name}::as_str);"));
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

/// Appends the test module.
fn tests(text: &mut String, out: &Output) {
    text.push_str(
        &TESTS
            .replace("@VALUES@", &out.values)
            .replace("@EXCLUDED@", &out.excluded)
            .replace("@CHECKS@", &out.checks.join("\n")),
    );
}
