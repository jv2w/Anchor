//! `AvailableCommands`: the commands a player may use, with their overloads, for the client to
//! list and autocomplete (PMMP `NetworkSession::syncAvailableCommands` and its packet assembler,
//! with typed overloads as dragonfly builds them).
//!
//! The network form of every command is built once, when the server is built ([`Catalog`]):
//! its enums (aliases, enum parameters and the item, block, effect and enchantment names) and
//! their values sit in shared tables, each once. A player's packet only picks the commands it
//! may use and renumbers the enums and values they use, in first-use order, copying their texts.
//!
//! Soft enums (BDS's dynamic enums) are named in the catalog; their values are the commands' state
//! ([`Listing::soft_values`]), set while the server runs, so each packet copies them.
//!
//! The names take BDS's shape (P6): a command's aliases enum is `<name>CommandAliases` with the
//! name first, and a `CommandName` enum lists every name and alias the player may type. `help` is
//! listed there but not sent as a command: the client has its own `/help`, and neither BDS nor PMMP
//! sends one. The catalog holds no handler, so it is `Sync`: the spawn window builds the packet in
//! the parallel output phase ([`Listing`]); a resend after a permission change is built at the end
//! of the tick's serial phases.

use std::collections::BTreeMap;

use super::access::Access;
use super::{Grants, allowed, has_permission_of};
use crate::block;
use crate::command::parse::{NAMESPACE, Names};
use crate::command::{self, Command, Error, Kind, Param};
use crate::permission::Permission;
use crate::protocol::consts::{CommandArg, CommandArgType, CommandPermissionLevel, ParamOption};
use crate::protocol::packet::{AvailableCommands, Packet};
use crate::protocol::{
    Command as NetCommand, CommandEnum, CommandOverload, CommandParameter, DynamicEnum,
};
use crate::tick::Player;
use crate::tick::queue::SessionId;

/// The alias offset of a command without aliases (PMMP and dragonfly send -1 as a u32).
const NO_ALIASES: u32 = u32::MAX;
/// A table position not given yet in a player's packet.
const UNSET: u32 = u32::MAX;
/// The enum of every command name and alias a player may type (BDS).
pub(crate) const COMMAND_NAME_ENUM: &str = "CommandName";
/// The suffix of a command's aliases enum after its name (BDS: `tellCommandAliases`).
const ALIASES_SUFFIX: &str = "CommandAliases";
/// The command the client runs itself: listed in [`COMMAND_NAME_ENUM`], never sent.
const CLIENT_COMMAND: &str = "help";

/// The network form of every registered command, built once at build.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    /// Every enum value once, in first-use order over all commands.
    values: Vec<String>,
    /// Every enum once, each type name once.
    enums: Vec<Enum>,
    /// The commands in registration order.
    commands: Vec<Entry>,
    /// The number suffixes (`/xp`'s `l`), each once; few, so every packet carries them all.
    suffixes: Vec<String>,
    /// Every soft enum's type name once, in first-use order over all commands.
    soft: Vec<String>,
    /// Soft enums by type name.
    soft_index: BTreeMap<String, u32>,
}

/// An enum of the catalog: its type name and its values' positions in [`Catalog::values`].
#[derive(Debug)]
struct Enum {
    name: String,
    values: Vec<u32>,
}

/// A command of the catalog.
#[derive(Debug)]
struct Entry {
    name: String,
    description: String,
    /// A player needs one of them; none: everyone.
    permissions: Vec<Permission>,
    /// The name and the aliases, as positions in [`Catalog::values`].
    names: Vec<u32>,
    /// Whether the command is sent; the client's own command is only named.
    sent: bool,
    /// The aliases enum (the name and the aliases), if the command has aliases.
    aliases: Option<u32>,
    overloads: Vec<Vec<Parameter>>,
    /// The enums the command uses, each once, in the order a packet meets them: the aliases,
    /// then the parameters in order.
    enums: Vec<u32>,
    /// The soft enums the command uses, each once, in parameter order.
    soft: Vec<u32>,
}

/// A parameter of the catalog.
#[derive(Debug)]
struct Parameter {
    name: String,
    r#type: Type,
    optional: bool,
    /// `ParamOption` bits: BDS marks its item and block names as checked by the server.
    options: u8,
}

/// A parameter's network type.
#[derive(Debug, Clone, Copy)]
enum Type {
    /// `VALID | type`.
    Basic(CommandArgType),
    /// `VALID | ENUM | index`, the index the enum's in the catalog.
    Enum(u32),
    /// `SUFFIXED | index`, the suffix's index (BDS sets no `VALID` bit).
    Suffixed(u32),
    /// `VALID | SOFT_ENUM | index`, the index the soft enum's in the catalog.
    Soft(u32),
}

impl Catalog {
    /// The network form of `commands`, with the client's name enums from `names`. Fails if two
    /// enums share a type name but not their values, or a soft enum shares one with an enum: the
    /// client would get only the first.
    pub(crate) fn new(commands: &[Command], names: &Names) -> command::Result<Self> {
        let mut b = Builder::default();
        // A parameter may not take the name of the enum every packet builds itself.
        b.enum_index.insert(COMMAND_NAME_ENUM.to_owned(), UNSET);
        for command in commands {
            let mut used = Vec::new();
            // The registry keeps the name out of the aliases.
            let typed: Vec<&str> = std::iter::once(command.name.as_str())
                .chain(command.aliases.iter().map(String::as_str))
                .collect();
            let aliases = (typed.len() > 1).then(|| {
                let index = b.enumeration(
                    &format!("{}{ALIASES_SUFFIX}", command.name),
                    typed.iter().copied(),
                );
                used.push(index);
                index
            });
            let typed = typed.into_iter().map(|n| b.value(n)).collect();
            let mut soft = Vec::new();
            let overloads = command
                .overloads
                .iter()
                .map(|o| {
                    o.params
                        .iter()
                        .map(|p| {
                            let r#type = b.parameter(names, p);
                            match r#type {
                                Type::Enum(index) if !used.contains(&index) => used.push(index),
                                Type::Soft(index) if !soft.contains(&index) => soft.push(index),
                                _ => {}
                            }
                            let options = match p.kind {
                                Kind::Item | Kind::Block => {
                                    u8::from(ParamOption::HasSemanticConstraint)
                                }
                                _ => 0,
                            };
                            Parameter {
                                name: p.name.clone(),
                                r#type,
                                optional: p.optional,
                                options,
                            }
                        })
                        .collect()
                })
                .collect();
            b.catalog.commands.push(Entry {
                name: command.name.clone(),
                description: command.description.clone(),
                permissions: command.permissions.clone(),
                names: typed,
                sent: command.name != CLIENT_COMMAND,
                aliases,
                overloads,
                enums: used,
                soft,
            });
        }
        if let Some(name) = b
            .catalog
            .soft
            .iter()
            .find(|n| b.enum_index.contains_key(*n))
        {
            b.error
                .get_or_insert(Error::ConflictingEnum { name: name.clone() });
        }
        b.error.map_or(Ok(b.catalog), Err)
    }

    /// The position of the soft enum of type `name` among [`Self::soft_count`].
    pub(crate) fn soft_enum(&self, name: &str) -> Option<usize> {
        self.soft_index.get(name).map(|&index| position(index))
    }

    /// How many soft enum types the commands use.
    pub(crate) fn soft_count(&self) -> usize {
        self.soft.len()
    }

    /// The type name of soft enum `index`.
    pub(crate) fn soft_name(&self, index: usize) -> Option<&str> {
        self.soft.get(index).map(String::as_str)
    }
}

/// What a player's `AvailableCommands` is made from: the catalog, the permission grants, the
/// operator list and the soft enums' values. All of it is `Sync`, unlike the commands' handlers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Listing<'a> {
    pub(super) catalog: &'a Catalog,
    pub(super) grants: &'a Grants,
    pub(super) access: &'a Access,
    /// The values of each soft enum of the catalog, by its position.
    pub(super) soft_values: &'a [Vec<String>],
}

/// The `AvailableCommands` packet for `player` (of `session`): every command whose permission it
/// has, in registration order, with only the enums and values those commands use.
pub(crate) fn available_commands(
    listing: Listing<'_>,
    session: SessionId,
    player: &Player,
) -> Packet {
    let Listing {
        catalog,
        grants,
        access,
        soft_values,
    } = listing;
    let op = access.is_op(&player.identity.name);
    let mut enum_at = vec![UNSET; catalog.enums.len()];
    let mut soft_at = vec![UNSET; catalog.soft.len()];
    let mut dynamic_enums = Vec::new();
    let mut value_at = vec![UNSET; catalog.values.len()];
    let mut values = Vec::new();
    let mut enums = Vec::new();
    let mut commands = Vec::new();
    let mut names = Vec::with_capacity(catalog.commands.len());
    for entry in &catalog.commands {
        if !allowed(&entry.permissions, |p| {
            has_permission_of(grants, Some(session), op, p)
        }) {
            continue;
        }
        names.extend(&entry.names);
        if !entry.sent {
            continue;
        }
        for &index in &entry.enums {
            let (Some(at), Some(e)) = (
                slot(&mut enum_at, index),
                catalog.enums.get(position(index)),
            ) else {
                continue;
            };
            if *at == UNSET {
                *at = index_of(enums.len());
                let value_indices = e
                    .values
                    .iter()
                    .map(|&v| value_of(catalog, &mut value_at, &mut values, v))
                    .collect();
                enums.push(CommandEnum {
                    r#type: e.name.clone(),
                    value_indices,
                });
            }
        }
        for &index in &entry.soft {
            soft_enum_of(
                catalog,
                soft_values,
                &mut soft_at,
                &mut dynamic_enums,
                index,
            );
        }
        let overloads = entry
            .overloads
            .iter()
            .map(|params| CommandOverload {
                chaining: false,
                parameters: params
                    .iter()
                    .map(|p| CommandParameter {
                        name: p.name.clone(),
                        r#type: wire_type(p.r#type, &enum_at, &soft_at),
                        optional: p.optional,
                        options: p.options,
                    })
                    .collect(),
            })
            .collect();
        commands.push(NetCommand {
            name: entry.name.clone(),
            description: entry.description.clone(),
            flags: 0,
            // The client leaves permission checks to the server.
            permission_level: u8::from(CommandPermissionLevel::Any),
            aliases_offset: entry
                .aliases
                .map_or(NO_ALIASES, |index| renumbered(&enum_at, index)),
            chained_subcommand_offsets: Vec::new(),
            overloads,
        });
    }
    let value_indices = names
        .into_iter()
        .map(|v| value_of(catalog, &mut value_at, &mut values, v))
        .collect();
    enums.push(CommandEnum {
        r#type: COMMAND_NAME_ENUM.to_owned(),
        value_indices,
    });
    Packet::AvailableCommands(Box::new(AvailableCommands {
        enum_values: values,
        chained_subcommand_values: Vec::new(),
        suffixes: catalog.suffixes.clone(),
        enums,
        chained_subcommands: Vec::new(),
        commands,
        dynamic_enums,
        constraints: Vec::new(),
    }))
}

/// A parameter's packet type, its enum renumbered by `enum_at` or `soft_at`.
fn wire_type(r#type: Type, enum_at: &[u32], soft_at: &[u32]) -> u32 {
    match r#type {
        Type::Basic(t) => u32::from(CommandArg::Valid) | u32::from(t),
        Type::Enum(index) => {
            u32::from(CommandArg::Valid) | u32::from(CommandArg::Enum) | renumbered(enum_at, index)
        }
        Type::Suffixed(index) => u32::from(CommandArg::Suffixed) | index,
        Type::Soft(index) => {
            u32::from(CommandArg::Valid)
                | u32::from(CommandArg::SoftEnum)
                | renumbered(soft_at, index)
        }
    }
}

/// The packet position `at` gives catalog position `index`.
fn renumbered(at: &[u32], index: u32) -> u32 {
    at.get(position(index)).copied().unwrap_or(UNSET)
}

/// Copies catalog soft enum `index` with its values into `dynamic_enums` on first use.
fn soft_enum_of(
    catalog: &Catalog,
    soft_values: &[Vec<String>],
    soft_at: &mut [u32],
    dynamic_enums: &mut Vec<DynamicEnum>,
    index: u32,
) {
    let (Some(at), Some(name)) = (slot(soft_at, index), catalog.soft.get(position(index))) else {
        return;
    };
    if *at != UNSET {
        return;
    }
    *at = index_of(dynamic_enums.len());
    dynamic_enums.push(DynamicEnum {
        r#type: name.clone(),
        values: soft_values
            .get(position(index))
            .cloned()
            .unwrap_or_default(),
    });
}

/// The packet position of catalog value `v`, copied into `values` on first use.
fn value_of(catalog: &Catalog, value_at: &mut [u32], values: &mut Vec<String>, v: u32) -> u32 {
    match slot(value_at, v) {
        Some(at) if *at == UNSET => {
            *at = index_of(values.len());
            values.push(catalog.values.get(position(v)).cloned().unwrap_or_default());
            *at
        }
        Some(at) => *at,
        None => UNSET,
    }
}

/// The entry of `table` at catalog position `index`.
fn slot(table: &mut [u32], index: u32) -> Option<&mut u32> {
    table.get_mut(position(index))
}

/// A catalog position as a slice index; positions come from `usize` lengths, so they fit.
fn position(index: u32) -> usize {
    usize::try_from(index).unwrap_or(usize::MAX)
}

/// The catalog as it fills.
#[derive(Default)]
struct Builder {
    catalog: Catalog,
    value_index: BTreeMap<String, u32>,
    /// Enums by type name.
    enum_index: BTreeMap<String, u32>,
    /// The first enum whose type name an enum of other values has.
    error: Option<Error>,
}

impl Builder {
    /// The index of the enum `name` with `values`, added on first use. An enum of the name with
    /// other values is an error.
    fn enumeration<'v>(&mut self, name: &str, values: impl IntoIterator<Item = &'v str>) -> u32 {
        let values: Vec<u32> = values.into_iter().map(|v| self.value(v)).collect();
        if let Some(&index) = self.enum_index.get(name) {
            // A reserved name has no enum of the catalog: any values conflict.
            if self
                .catalog
                .enums
                .get(position(index))
                .is_none_or(|e| e.values != values)
            {
                self.error.get_or_insert(Error::ConflictingEnum {
                    name: name.to_owned(),
                });
            }
            return index;
        }
        let index = index_of(self.catalog.enums.len());
        self.catalog.enums.push(Enum {
            name: name.to_owned(),
            values,
        });
        self.enum_index.insert(name.to_owned(), index);
        index
    }

    fn value(&mut self, value: &str) -> u32 {
        if let Some(&index) = self.value_index.get(value) {
            return index;
        }
        let index = index_of(self.catalog.values.len());
        self.catalog.values.push(value.to_owned());
        self.value_index.insert(value.to_owned(), index);
        index
    }

    /// The index of `suffix`, added on first use.
    fn suffix(&mut self, suffix: &str) -> u32 {
        let suffixes = &mut self.catalog.suffixes;
        if let Some(i) = suffixes.iter().position(|s| s == suffix) {
            return index_of(i);
        }
        suffixes.push(suffix.to_owned());
        index_of(suffixes.len() - 1)
    }

    /// The index of the soft enum `name`, added on first use.
    fn soft(&mut self, name: &str) -> u32 {
        if let Some(&index) = self.catalog.soft_index.get(name) {
            return index;
        }
        let index = index_of(self.catalog.soft.len());
        self.catalog.soft.push(name.to_owned());
        self.catalog.soft_index.insert(name.to_owned(), index);
        index
    }

    /// The network type of `param` as BDS sends it (P6): a basic type, an enum (`VALID | type` or
    /// `VALID | ENUM | index`, as PMMP's assembler) or a suffixed number. Item and block names come
    /// with and without `minecraft:`.
    fn parameter(&mut self, names: &Names, param: &Param) -> Type {
        match &param.kind {
            Kind::Int => Type::Basic(CommandArgType::Int),
            Kind::Suffixed { suffix } => Type::Suffixed(self.suffix(suffix)),
            Kind::Float => Type::Basic(CommandArgType::Float),
            Kind::String => Type::Basic(CommandArgType::String),
            Kind::Target | Kind::Name => Type::Basic(CommandArgType::Target),
            Kind::Position => Type::Basic(CommandArgType::Position),
            // BDS and PMMP (`ARG_TYPE_MESSAGE`) send 68, gophertunnel's `Message` is 67.
            Kind::Message => Type::Basic(CommandArgType::MessageRoot),
            Kind::RawText => Type::Basic(CommandArgType::RawText),
            Kind::Json => Type::Basic(CommandArgType::Json),
            Kind::Bool => Type::Enum(self.enumeration(command::BOOLEAN_ENUM, ["true", "false"])),
            Kind::Enum { name, values } => {
                Type::Enum(self.enumeration(name, values.iter().map(String::as_str)))
            }
            Kind::Item => {
                let values = namespaced(names.item_names().iter().copied());
                Type::Enum(self.enumeration(command::ITEM_ENUM, values.iter().map(String::as_str)))
            }
            Kind::Block => {
                let values = namespaced(block::Kind::all().iter().map(|k| k.name()));
                Type::Enum(self.enumeration(command::BLOCK_ENUM, values.iter().map(String::as_str)))
            }
            Kind::Effect => {
                Type::Enum(self.enumeration(command::EFFECT_ENUM, names.effect_names()))
            }
            Kind::Enchantment => {
                Type::Enum(self.enumeration(command::ENCHANT_ENUM, names.enchantment_names()))
            }
            Kind::SoftEnum { name } => Type::Soft(self.soft(name)),
            Kind::ClockTimeMarker => Type::Basic(CommandArgType::ClockTimeMarker),
        }
    }
}

/// Each of `names` as `minecraft:<name>` and as itself, BDS's order.
fn namespaced<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    names
        .flat_map(|n| [format!("{NAMESPACE}{n}"), n.to_owned()])
        .collect()
}

/// A list index as the packet's u32. Lists here are far below 2^32 entries (bounded command,
/// overload and enum counts).
fn index_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
