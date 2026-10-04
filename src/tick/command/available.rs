//! `AvailableCommands`: the commands a player may use, with their overloads, for the client to
//! list and autocomplete (PMMP `NetworkSession::syncAvailableCommands` and its packet assembler,
//! with typed overloads as dragonfly builds them).
//!
//! The network form of every command is built once, when the server is built ([`Catalog`]):
//! its enums (aliases, enum parameters and the item, block, effect and enchantment names) and
//! their values sit in shared tables, each once. A player's packet only picks the commands it
//! may use and renumbers the enums and values they use, in first-use order, copying their texts.
//! The catalog holds no handler, so it is `Sync`: the spawn window builds the packet in the
//! parallel output phase ([`Listing`]); a resend after a permission change is built at the end of
//! the tick's serial phases.

use std::collections::BTreeMap;

use super::access::Access;
use super::{Grants, allowed, has_permission_of};
use crate::block;
use crate::command::parse::Names;
use crate::command::{Command, Kind, Param};
use crate::permission::Permission;
use crate::protocol::consts::{CommandArg, CommandArgType};
use crate::protocol::packet::{AvailableCommands, Packet};
use crate::protocol::{Command as NetCommand, CommandEnum, CommandOverload, CommandParameter};
use crate::tick::Player;
use crate::tick::queue::SessionId;

/// The alias offset of a command without aliases (PMMP and dragonfly send -1 as a u32).
const NO_ALIASES: u32 = u32::MAX;
/// `CommandPermissionLevelAny`: the client leaves permission checks to the server.
const PERMISSION_ANY: u8 = 0;
/// A table position not given yet in a player's packet.
const UNSET: u32 = u32::MAX;

/// The network form of every registered command, built once at build.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    /// Every enum value once, in first-use order over all commands.
    values: Vec<String>,
    /// Every enum once: the first enum of a type name is the one clients get.
    enums: Vec<Enum>,
    /// The commands in registration order.
    commands: Vec<Entry>,
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
    /// The aliases enum (the aliases and the name), if the command has aliases.
    aliases: Option<u32>,
    overloads: Vec<Vec<Parameter>>,
    /// The enums the command uses, each once, in the order a packet meets them: the aliases,
    /// then the parameters in order.
    enums: Vec<u32>,
}

/// A parameter of the catalog.
#[derive(Debug)]
struct Parameter {
    name: String,
    r#type: Type,
    optional: bool,
}

/// A parameter's network type.
#[derive(Debug, Clone, Copy)]
enum Type {
    /// `VALID | type`.
    Basic(CommandArgType),
    /// `VALID | ENUM | index`, the index the enum's in the catalog.
    Enum(u32),
}

impl Catalog {
    /// The network form of `commands`, with the client's name enums from `names`.
    pub(crate) fn new(commands: &[Command], names: &Names) -> Self {
        let mut b = Builder::default();
        for command in commands {
            let mut used = Vec::new();
            let aliases = (!command.aliases.is_empty()).then(|| {
                // PMMP adds the name to its aliases: the client hides the name otherwise.
                let mut values: Vec<&str> = command.aliases.iter().map(String::as_str).collect();
                if !values.contains(&command.name.as_str()) {
                    values.push(&command.name);
                }
                let index =
                    b.enumeration(&format!("{}Aliases", capitalised(&command.name)), values);
                used.push(index);
                index
            });
            let overloads = command
                .overloads
                .iter()
                .map(|o| {
                    o.params
                        .iter()
                        .map(|p| {
                            let r#type = b.parameter(names, p);
                            if let Type::Enum(index) = r#type
                                && !used.contains(&index)
                            {
                                used.push(index);
                            }
                            Parameter {
                                name: p.name.clone(),
                                r#type,
                                optional: p.optional,
                            }
                        })
                        .collect()
                })
                .collect();
            b.catalog.commands.push(Entry {
                name: command.name.clone(),
                description: command.description.clone(),
                permissions: command.permissions.clone(),
                aliases,
                overloads,
                enums: used,
            });
        }
        b.catalog
    }
}

/// What a player's `AvailableCommands` is made from: the catalog, the permission grants and the
/// operator list. All of it is `Sync`, unlike the commands' handlers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Listing<'a> {
    pub(super) catalog: &'a Catalog,
    pub(super) grants: &'a Grants,
    pub(super) access: &'a Access,
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
    } = listing;
    let op = access.is_op(&player.identity.name);
    let mut enum_at = vec![UNSET; catalog.enums.len()];
    let mut value_at = vec![UNSET; catalog.values.len()];
    let mut values = Vec::new();
    let mut enums = Vec::new();
    let mut commands = Vec::new();
    for entry in &catalog.commands {
        if !allowed(&entry.permissions, |p| {
            has_permission_of(grants, Some(session), op, p)
        }) {
            continue;
        }
        for &index in &entry.enums {
            let (Some(at), Some(e)) = (
                slot(&mut enum_at, index),
                catalog.enums.get(position(index)),
            ) else {
                continue;
            };
            if *at != UNSET {
                continue;
            }
            *at = index_of(enums.len());
            let value_indices = e
                .values
                .iter()
                .map(|&v| match slot(&mut value_at, v) {
                    Some(at) if *at == UNSET => {
                        *at = index_of(values.len());
                        values.push(catalog.values.get(position(v)).cloned().unwrap_or_default());
                        *at
                    }
                    Some(at) => *at,
                    None => UNSET,
                })
                .collect();
            enums.push(CommandEnum {
                r#type: e.name.clone(),
                value_indices,
            });
        }
        let renumbered = |index: u32| enum_at.get(position(index)).copied().unwrap_or(UNSET);
        let overloads = entry
            .overloads
            .iter()
            .map(|params| CommandOverload {
                chaining: false,
                parameters: params
                    .iter()
                    .map(|p| CommandParameter {
                        name: p.name.clone(),
                        r#type: match p.r#type {
                            Type::Basic(t) => u32::from(CommandArg::Valid) | u32::from(t),
                            Type::Enum(index) => {
                                u32::from(CommandArg::Valid)
                                    | u32::from(CommandArg::Enum)
                                    | renumbered(index)
                            }
                        },
                        optional: p.optional,
                        options: 0,
                    })
                    .collect(),
            })
            .collect();
        commands.push(NetCommand {
            name: entry.name.clone(),
            description: entry.description.clone(),
            flags: 0,
            permission_level: PERMISSION_ANY,
            aliases_offset: entry.aliases.map_or(NO_ALIASES, renumbered),
            chained_subcommand_offsets: Vec::new(),
            overloads,
        });
    }
    Packet::AvailableCommands(Box::new(AvailableCommands {
        enum_values: values,
        chained_subcommand_values: Vec::new(),
        suffixes: Vec::new(),
        enums,
        chained_subcommands: Vec::new(),
        commands,
        dynamic_enums: Vec::new(),
        constraints: Vec::new(),
    }))
}

/// The entry of `table` at catalog position `index`.
fn slot(table: &mut [u32], index: u32) -> Option<&mut u32> {
    table.get_mut(position(index))
}

/// A catalog position as a slice index; positions come from `usize` lengths, so they fit.
fn position(index: u32) -> usize {
    usize::try_from(index).unwrap_or(usize::MAX)
}

/// `heal` → `Heal` (PMMP `ucfirst`).
fn capitalised(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The catalog as it fills.
#[derive(Default)]
struct Builder {
    catalog: Catalog,
    value_index: BTreeMap<String, u32>,
    /// Enums by type name: the first enum of a name is the one the client gets.
    enum_index: BTreeMap<String, u32>,
}

impl Builder {
    /// The index of the enum `name` with `values`, added on first use.
    fn enumeration<'v>(&mut self, name: &str, values: impl IntoIterator<Item = &'v str>) -> u32 {
        if let Some(&index) = self.enum_index.get(name) {
            return index;
        }
        let values = values.into_iter().map(|v| self.value(v)).collect();
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

    /// The network type of `param`: a basic type, or an enum (PMMP's assembler: `VALID | type`
    /// or `VALID | ENUM | index`).
    fn parameter(&mut self, names: &Names, param: &Param) -> Type {
        match &param.kind {
            Kind::Int => Type::Basic(CommandArgType::Int),
            Kind::Float => Type::Basic(CommandArgType::Float),
            Kind::String => Type::Basic(CommandArgType::String),
            Kind::Target => Type::Basic(CommandArgType::Target),
            Kind::Position => Type::Basic(CommandArgType::Position),
            // BDS and PMMP (`ARG_TYPE_MESSAGE`) send 68, gophertunnel's `Message` is 67.
            Kind::Message => Type::Basic(CommandArgType::MessageRoot),
            Kind::RawText => Type::Basic(CommandArgType::RawText),
            Kind::Json => Type::Basic(CommandArgType::Json),
            Kind::Bool => Type::Enum(self.enumeration("Boolean", ["true", "false"])),
            Kind::Enum { name, values } => {
                Type::Enum(self.enumeration(name, values.iter().map(String::as_str)))
            }
            Kind::Item => Type::Enum(self.enumeration("Item", names.item_names().iter().copied())),
            Kind::Block => {
                Type::Enum(self.enumeration("Block", block::Kind::all().iter().map(|k| k.name())))
            }
            Kind::Effect => Type::Enum(self.enumeration("Effect", names.effect_names())),
            Kind::Enchantment => Type::Enum(self.enumeration("Enchant", names.enchantment_names())),
        }
    }
}

/// A list index as the packet's u32. Lists here are far below 2^32 entries (bounded command,
/// overload and enum counts).
fn index_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
