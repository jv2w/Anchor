//! Custom block definitions: the states, what the client is told to draw, the
//! server-side data the rules read, and the behavior.

use std::fmt;

use super::{Creative, ItemRef};
use crate::Context;
use crate::block::{self, BreakInfo, PropertyValue, ToolType};
use crate::math::{Aabb, Vec3, to_f32};
use crate::nbt::{Compound, List, Value};
use crate::player;
use crate::world::{self, BlockPos};

/// The Molang version custom blocks are sent with.
const MOLANG_VERSION: i32 = 10;

/// The pixels along one edge of a block, which the box components count in.
const PIXELS_PER_BLOCK: f64 = 16.0;

/// A custom block: a `static` definition the server is given with
/// [`ServerBuilder::custom_block`](crate::ServerBuilder::custom_block). Its textures and models
/// come from a resource pack, and so does its name: the client reads it from the pack's language
/// file as `tile.<identifier>.name`.
///
/// The definition gives the client the block's states (the product of its [`Property`]
/// values), shape, materials, light, break time, friction and flammability, and gives the server
/// the data its rules read: break info, solid and transparent, light, friction, collision box and
/// drops. The server gives the block an item, in the creative inventory if
/// [`with_creative`](Self::with_creative) says so.
///
/// ```
/// use anchor::custom::{Block, Category, Creative, Property};
///
/// static LAMP: Block = Block::new("example:lamp", "example_lamp")
///     .with_properties(&[Property::bool("lit")])
///     .with_light_emission(15)
///     .with_hardness(0.3)
///     .with_creative(Creative::new(Category::Construction));
/// ```
#[derive(Clone, Copy)]
pub struct Block {
    identifier: &'static str,
    /// The default material until `materials` replaces them.
    texture: Material,
    materials: Option<&'static [Material]>,
    properties: &'static [Property],
    geometry: Geometry,
    collision: Option<Aabb>,
    selection: Option<Aabb>,
    light_emission: u8,
    /// `None`: 15 for an opaque block, 0 for a transparent one.
    light_filter: Option<u8>,
    info: block::Info,
    flammability: u8,
    flame_encouragement: u8,
    drops: Drops,
    creative: Option<Creative>,
    behavior: Option<&'static dyn Behavior>,
    random_ticks: bool,
}

impl Block {
    /// The blast resistance of a block per point of hardness.
    const BLAST_RESISTANCE_PER_HARDNESS: f64 = 5.0;

    /// The friction of most blocks; the client's component leaves it out.
    const DEFAULT_FRICTION: f64 = 0.6;

    /// The hardness of a block made by [`new`](Self::new).
    const DEFAULT_HARDNESS: f64 = 1.0;

    /// The whole cell, a block's default collision and selection box.
    const FULL_CELL: Aabb = Aabb::new(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));

    /// The most drops one block lists.
    const MAX_DROPS: usize = 64;

    /// The highest friction.
    const MAX_FRICTION: f64 = 1.0;

    /// The highest harvest level, netherite.
    const MAX_HARVEST_LEVEL: u8 = 6;

    /// The highest light level.
    const MAX_LIGHT_LEVEL: u8 = 15;

    /// The most properties a custom block has.
    const MAX_PROPERTIES: usize = 16;

    /// The longest texture or geometry name, in bytes.
    const MAX_RESOURCE_NAME_BYTES: usize = 256;

    /// The most states one custom block has (each is a `u16` index).
    const MAX_STATES: u32 = 4096;

    /// The lowest friction: the client takes 1 minus the friction as its component, at most 0.9,
    /// so a lower one would slide differently there than in the server's rules.
    const MIN_FRICTION: f64 = 0.1;

    /// A full, opaque, solid cube with `texture` on every face, named `identifier`
    /// (`namespace:name`, not in `minecraft`): hardness 1 by hand, friction 0.6, no light,
    /// dropping itself, not in the creative inventory.
    pub const fn new(identifier: &'static str, texture: &'static str) -> Self {
        Self {
            identifier,
            texture: Material::new(texture),
            materials: None,
            properties: &[],
            geometry: Geometry::FullCube,
            collision: Some(Self::FULL_CELL),
            selection: Some(Self::FULL_CELL),
            light_emission: 0,
            light_filter: None,
            info: block::Info::custom(
                BreakInfo::new(
                    Self::DEFAULT_HARDNESS,
                    Self::DEFAULT_HARDNESS * Self::BLAST_RESISTANCE_PER_HARDNESS,
                    ToolType::NONE,
                    0,
                ),
                block::flag::SOLID,
                Self::DEFAULT_FRICTION,
                block::SUPPORT_FULL,
            ),
            flammability: 0,
            flame_encouragement: 0,
            drops: Drops::Itself,
            creative: None,
            behavior: None,
            random_ticks: false,
        }
    }

    /// Its behavior.
    pub(crate) fn behavior_hooks(&self) -> Option<&'static dyn Behavior> {
        self.behavior
    }

    /// Why the definition cannot be registered, if it cannot.
    pub(super) fn check(&self) -> Option<&'static str> {
        if self.properties.len() > Self::MAX_PROPERTIES {
            return Some("more than 16 properties");
        }
        if let Some(reason) = self.properties.iter().find_map(Property::check) {
            return Some(reason);
        }
        if self.properties.windows(2).any(|w| match w {
            [a, b] => a.name >= b.name,
            _ => false,
        }) {
            return Some("the properties are not in name order or a name is given twice");
        }
        if self.state_count() > Self::MAX_STATES {
            return Some("more than 4,096 states");
        }
        let materials = self.material_list();
        if materials.is_empty()
            || materials
                .iter()
                .any(|m| m.texture.is_empty() || m.texture.len() > Self::MAX_RESOURCE_NAME_BYTES)
        {
            return Some("no material, or a texture name that is empty or too long");
        }
        if !materials.iter().any(|m| m.target == "*") {
            return Some("no material for every face (`Material::new`)");
        }
        if materials.iter().enumerate().any(|(i, m)| {
            materials
                .get(..i)
                .is_some_and(|before| before.iter().any(|b| b.target == m.target))
        }) {
            return Some("a face has two materials");
        }
        if let Geometry::Model(g) = self.geometry
            && (g.is_empty() || g.len() > Self::MAX_RESOURCE_NAME_BYTES)
        {
            return Some("the geometry name is empty or too long");
        }
        let in_cell = |b: Option<Aabb>| {
            b.is_none_or(|b| {
                [b.min.x, b.min.y, b.min.z, b.max.x, b.max.y, b.max.z]
                    .iter()
                    .all(|c| c.is_finite() && (0.0..=1.0).contains(c))
                    && b.min.x <= b.max.x
                    && b.min.y <= b.max.y
                    && b.min.z <= b.max.z
            })
        };
        if !in_cell(self.collision) || !in_cell(self.selection) {
            return Some("a box is not inside the block's cell");
        }
        if self.light_emission > Self::MAX_LIGHT_LEVEL
            || self.light_filter.is_some_and(|l| l > Self::MAX_LIGHT_LEVEL)
        {
            return Some("a light level above 15");
        }
        let b = self.info.breaking();
        let hardness_ok = b.hardness() == -1.0 || (b.hardness().is_finite() && b.hardness() >= 0.0);
        if !hardness_ok || !b.blast_resistance().is_finite() || b.blast_resistance() < 0.0 {
            return Some("a hardness or blast resistance that is negative or not finite");
        }
        let friction = self.info.friction();
        if !(Self::MIN_FRICTION..=Self::MAX_FRICTION).contains(&friction) {
            return Some("a friction outside 0.1 to 1");
        }
        if b.harvest_level() > Self::MAX_HARVEST_LEVEL {
            return Some("a harvest level above 6 (netherite)");
        }
        // The count of a drop within its item's stack size is checked once the items are numbered.
        if let Drops::Items(items) = self.drops
            && (items.len() > Self::MAX_DROPS || items.iter().any(|(_, count)| *count == 0))
        {
            return Some("more than 64 drops, or a drop count of 0");
        }
        None
    }

    /// The collision boxes: none or one.
    pub(crate) fn collision_boxes(&'static self) -> &'static [Aabb] {
        self.collision.as_slice()
    }

    /// Where its item shows in the creative inventory.
    pub(super) fn creative_place(&self) -> Option<Creative> {
        self.creative
    }

    /// What breaking it drops.
    pub(crate) fn dropped(&self) -> Drops {
        self.drops
    }

    /// The light given off.
    pub(crate) fn emission(&self) -> u8 {
        self.light_emission
    }

    /// The block's `StartGame` entry properties: components, the creative menu, the block ID and
    /// the property list. `block_id` is the block's ID among the custom blocks' IDs.
    pub(super) fn entry_nbt(&'static self, block_id: i32) -> Compound {
        let (category, group) = match self.creative {
            Some(c) => (c.category.name(), c.group.unwrap_or_default()),
            None => ("none", ""),
        };
        let mut out = Compound::from([
            (
                "components".to_owned(),
                Value::Compound(self.to_components()),
            ),
            (
                "menu_category".to_owned(),
                Value::Compound(Compound::from([
                    ("category".to_owned(), Value::String(category.to_owned())),
                    ("group".to_owned(), Value::String(group.to_owned())),
                ])),
            ),
            ("molangVersion".to_owned(), Value::Int(MOLANG_VERSION)),
            (
                "vanilla_block_data".to_owned(),
                Value::Compound(Compound::from([(
                    "block_id".to_owned(),
                    Value::Int(block_id),
                )])),
            ),
        ]);
        if !self.properties.is_empty() {
            let list = self.properties.iter().map(Property::nbt).collect();
            out.insert("properties".to_owned(), Value::List(List::Compound(list)));
        }
        out
    }

    /// The light filter.
    pub(crate) fn filter(&self) -> u8 {
        match self.light_filter {
            Some(level) => level,
            None if self.info.has_flag(block::flag::TRANSPARENT) => 0,
            None => Self::MAX_LIGHT_LEVEL,
        }
    }

    /// Whether it takes random ticks.
    pub(crate) fn has_random_ticks(&self) -> bool {
        self.random_ticks
    }

    /// The identifier, such as `example:lamp`.
    pub const fn identifier(&self) -> &'static str {
        self.identifier
    }

    /// The state whose properties are exactly `states`, a disk palette entry's `states` (in name
    /// order, as the properties are); `None` for a missing, extra or repeated property, an
    /// unknown value or a value of another type.
    pub(super) fn index_of(&self, states: &Compound) -> Option<u16> {
        if states.len() != self.properties.len() {
            return None;
        }
        let mut index = 0_u32;
        let mut scale = 1_u32;
        for (p, (name, value)) in self.properties.iter().zip(states) {
            if p.name != name.as_str() {
                return None;
            }
            let value = match value {
                Value::Byte(v) => PropertyValue::Byte(*v),
                Value::Int(v) => PropertyValue::Int(*v),
                Value::String(v) => PropertyValue::String(v),
                _ => return None,
            };
            index = index.checked_add(p.position(value)?.checked_mul(scale)?)?;
            scale = scale.checked_mul(p.count())?;
        }
        u16::try_from(index).ok()
    }

    /// The physics row the rules read.
    pub(crate) fn info(&'static self) -> &'static block::Info {
        &self.info
    }

    /// How many properties the block has.
    pub(crate) fn property_count(&self) -> usize {
        self.properties.len()
    }

    /// The name of property `at`.
    pub(crate) fn property_name(&self, at: usize) -> Option<&'static str> {
        self.properties.get(at).map(|p| p.name)
    }

    /// The value of property `at` in the state `index` (properties in definition order, the
    /// first changing fastest).
    pub(crate) fn property_value(&self, index: u16, at: usize) -> Option<PropertyValue<'static>> {
        let mut rest = u32::from(index);
        for p in self.properties.get(..at)? {
            rest /= p.count().max(1);
        }
        let p = self.properties.get(at)?;
        p.value(rest % p.count().max(1))
    }

    /// How many states the block has: the product of its properties' value counts.
    pub(crate) fn state_count(&self) -> u32 {
        self.properties
            .iter()
            .try_fold(1_u32, |n, p| n.checked_mul(p.count()))
            .unwrap_or(u32::MAX)
    }

    /// The state `index` with property `name` set to `value`.
    pub(crate) fn with_value(
        &self,
        index: u16,
        name: &str,
        value: PropertyValue<'_>,
    ) -> Option<u16> {
        let mut scale = 1_u32;
        for p in self.properties {
            let count = p.count().max(1);
            if p.name == name {
                let current = u32::from(index) / scale % count;
                let new = p.position(value)?;
                let base = u32::from(index).checked_sub(current.checked_mul(scale)?)?;
                return u16::try_from(base.checked_add(new.checked_mul(scale)?)?).ok();
            }
            scale = scale.checked_mul(count)?;
        }
        None
    }

    /// The same block that entities can stand inside, such as a plant.
    #[must_use]
    pub const fn non_solid(mut self) -> Self {
        self.info = self.info.with_flag(block::flag::SOLID, false);
        self
    }

    /// The same block taking random ticks, which call its behavior's
    /// [`on_random_tick`](Behavior::on_random_tick). Blocks that do not need them cost nothing
    /// on random ticks without this.
    #[must_use]
    pub const fn random_ticks(mut self) -> Self {
        self.random_ticks = true;
        self
    }

    /// The same block letting light through and not hiding the faces next to it.
    #[must_use]
    pub const fn transparent(mut self) -> Self {
        self.info = self.info.with_flag(block::flag::TRANSPARENT, true);
        self
    }

    /// The same block that nothing breaks (hardness -1).
    #[must_use]
    pub const fn unbreakable(mut self) -> Self {
        let b = self.info.breaking();
        self.info = self.info.with_breaking(BreakInfo::new(
            -1.0,
            b.blast_resistance(),
            b.tool_type(),
            b.harvest_level(),
        ));
        self
    }

    /// The same block acting with `behavior`.
    #[must_use]
    pub const fn with_behavior(mut self, behavior: &'static dyn Behavior) -> Self {
        self.behavior = Some(behavior);
        self
    }

    /// The same block with this blast resistance.
    #[must_use]
    pub const fn with_blast_resistance(mut self, resistance: f64) -> Self {
        let b = self.info.breaking();
        self.info = self.info.with_breaking(BreakInfo::new(
            b.hardness(),
            resistance,
            b.tool_type(),
            b.harvest_level(),
        ));
        self
    }

    /// The same block with this collision box (corners from 0 to 1 in each axis), or none
    /// (entities pass through it). The block gives full support to its neighbours only with the
    /// whole cell.
    #[must_use]
    pub const fn with_collision_box(mut self, collision: Option<Aabb>) -> Self {
        self.collision = collision;
        let support = match collision {
            Some(b) if is_full(b) => block::SUPPORT_FULL,
            _ => block::SUPPORT_NONE,
        };
        self.info = self.info.with_support(support);
        self
    }

    /// The same block whose item is in the creative inventory.
    #[must_use]
    pub const fn with_creative(mut self, creative: Creative) -> Self {
        self.creative = Some(creative);
        self
    }

    /// The same block dropping `drops`.
    #[must_use]
    pub const fn with_drops(mut self, drops: Drops) -> Self {
        self.drops = drops;
        self
    }

    /// The same block telling the client it catches fire with chance `flammability` and helps
    /// fire spread by `encouragement` (`minecraft:flammable`); the server spreads no fire.
    #[must_use]
    pub const fn with_flammability(mut self, flammability: u8, encouragement: u8) -> Self {
        self.flammability = flammability;
        self.flame_encouragement = encouragement;
        self
    }

    /// The same block with this friction factor, 0.1 to 1 (0.6 for most blocks, 0.98 for ice); a
    /// block with another factor is refused when the server is built.
    #[must_use]
    pub const fn with_friction(mut self, friction: f64) -> Self {
        self.info = self.info.with_friction(friction);
        self
    }

    /// The same block drawn with the resource pack model `geometry` (such as `geometry.lamp`)
    /// instead of a cube. Give it the collision and selection boxes of the model.
    #[must_use]
    pub const fn with_geometry(mut self, geometry: &'static str) -> Self {
        self.geometry = Geometry::Model(geometry);
        self
    }

    /// The same block taking `hardness` to break (the seconds are 1.5 times it with a compatible
    /// tool, 5 times without), with a blast resistance of 5 times it that replaces one given
    /// before ([`with_blast_resistance`](Self::with_blast_resistance) after it keeps its own). A
    /// block nothing breaks is [`unbreakable`](Self::unbreakable), not a hardness of -1.
    #[must_use]
    pub const fn with_hardness(mut self, hardness: f64) -> Self {
        let b = self.info.breaking();
        self.info = self.info.with_breaking(BreakInfo::new(
            hardness,
            hardness * Self::BLAST_RESISTANCE_PER_HARDNESS,
            b.tool_type(),
            b.harvest_level(),
        ));
        self
    }

    /// The same block giving off light of `level`, 0 to 15.
    #[must_use]
    pub const fn with_light_emission(mut self, level: u8) -> Self {
        self.light_emission = level;
        self
    }

    /// The same block reducing light through it by `level`, 0 to 15 (default 15 for an opaque
    /// block, 0 for a transparent one).
    #[must_use]
    pub const fn with_light_filter(mut self, level: u8) -> Self {
        self.light_filter = Some(level);
        self
    }

    /// The same block drawn with these materials (a `*` material and any per-face ones, at most
    /// one for each).
    #[must_use]
    pub const fn with_materials(mut self, materials: &'static [Material]) -> Self {
        self.materials = Some(materials);
        self
    }

    /// The same block with these properties, which must be in name order with no name twice;
    /// they make its states (the first value of each is the default state).
    #[must_use]
    pub const fn with_properties(mut self, properties: &'static [Property]) -> Self {
        self.properties = properties;
        self
    }

    /// The same block with this selection box (corners from 0 to 1), or none.
    #[must_use]
    pub const fn with_selection_box(mut self, selection: Option<Aabb>) -> Self {
        self.selection = selection;
        self
    }

    /// The same block needing `tool` and a tool of at least `harvest_level` (wood 1 to
    /// netherite 6; 0: any) to drop anything. The server breaks it faster with `tool`;
    /// the client is sent the time by hand only (it has no per-tool component), so a player
    /// waits that long for the block to break on screen.
    #[must_use]
    pub const fn with_tool(mut self, tool: ToolType, harvest_level: u8) -> Self {
        let b = self.info.breaking();
        self.info = self.info.with_breaking(BreakInfo::new(
            b.hardness(),
            b.blast_resistance(),
            tool,
            harvest_level,
        ));
        self
    }

    /// The materials it is drawn with.
    fn material_list(&self) -> &[Material] {
        match self.materials {
            Some(m) => m,
            None => std::slice::from_ref(&self.texture),
        }
    }

    /// The block's components: boxes, geometry, materials, light, break time, friction,
    /// flammability.
    fn to_components(self) -> Compound {
        let mut components = Compound::new();
        let mut put = |key: &str, value: Compound| {
            components.insert(key.to_owned(), Value::Compound(value));
        };
        match self.collision {
            Some(b) => put("minecraft:collision_box", collision_box(b)),
            None => put(
                "minecraft:collision_box",
                Compound::from([
                    ("boxes".to_owned(), Value::List(List::Compound(Vec::new()))),
                    ("enabled".to_owned(), Value::Byte(0)),
                ]),
            ),
        }
        match self.selection {
            Some(b) => put("minecraft:selection_box", selection_box(b)),
            None => put(
                "minecraft:selection_box",
                Compound::from([("enabled".to_owned(), Value::Byte(0))]),
            ),
        }
        let geometry = match self.geometry {
            Geometry::FullCube => "minecraft:geometry.full_block",
            Geometry::Model(g) => g,
        };
        put(
            "minecraft:geometry",
            Compound::from([("identifier".to_owned(), Value::String(geometry.to_owned()))]),
        );
        let materials = self
            .material_list()
            .iter()
            .map(|m| (m.target.to_owned(), Value::Compound(m.nbt())))
            .collect();
        put(
            "minecraft:material_instances",
            Compound::from([
                ("mappings".to_owned(), Value::Compound(Compound::new())),
                ("materials".to_owned(), Value::Compound(materials)),
            ]),
        );
        put(
            "minecraft:light_emission",
            Compound::from([(
                "emission".to_owned(),
                Value::Int(i32::from(self.light_emission)),
            )]),
        );
        put(
            "minecraft:light_dampening",
            Compound::from([(
                "lightLevel".to_owned(),
                Value::Int(i32::from(self.filter())),
            )]),
        );
        let breaking = self.info.breaking();
        if let Some(seconds) = breaking.break_time_secs(&crate::item::Stack::EMPTY) {
            // The seconds by hand: the client has no per-tool component.
            put(
                "minecraft:destructible_by_mining",
                Compound::from([("value".to_owned(), Value::Float(to_f32(seconds)))]),
            );
        }
        // The client's friction is the other way round: 0.4 for the server's 0.6.
        let friction = self.info.friction();
        if friction != Self::DEFAULT_FRICTION {
            let value = to_f32(1.0 - friction);
            put(
                "minecraft:friction",
                Compound::from([("value".to_owned(), Value::Float(value))]),
            );
        }
        if self.flammability > 0 {
            put(
                "minecraft:flammable",
                Compound::from([
                    (
                        "burn_odds".to_owned(),
                        Value::Int(i32::from(self.flammability)),
                    ),
                    (
                        "flame_odds".to_owned(),
                        Value::Int(i32::from(self.flame_encouragement)),
                    ),
                ]),
            );
        }
        components
    }
}

impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Block")
            .field("identifier", &self.identifier)
            .field("texture", &self.texture)
            .field("materials", &self.materials)
            .field("properties", &self.properties)
            .field("geometry", &self.geometry)
            .field("collision", &self.collision)
            .field("selection", &self.selection)
            .field("light_emission", &self.light_emission)
            .field("light_filter", &self.light_filter)
            .field("info", &self.info)
            .field("flammability", &self.flammability)
            .field("flame_encouragement", &self.flame_encouragement)
            .field("drops", &self.drops)
            .field("creative", &self.creative)
            .field("behavior", &self.behavior.is_some())
            .field("random_ticks", &self.random_ticks)
            .finish()
    }
}

/// A property of a custom block, which with the others makes its states: a block with
/// properties `lit` (a flag) and `level` (0 to 3) has eight states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Property {
    name: &'static str,
    values: Values,
}

impl Property {
    /// The longest property name or value, in bytes.
    const MAX_NAME_BYTES: usize = 64;

    /// The most values a custom block property has.
    const MAX_VALUES: u32 = 16;

    /// A property that is false or true; false in the default state.
    pub const fn bool(name: &'static str) -> Self {
        Self {
            name,
            values: Values::Bool,
        }
    }

    /// A property with these words; the first in the default state.
    pub const fn enumeration(name: &'static str, values: &'static [&'static str]) -> Self {
        Self {
            name,
            values: Values::Text(values),
        }
    }

    /// A property with the whole numbers `min` to `max`; `min` in the default state.
    pub const fn int(name: &'static str, min: i32, max: i32) -> Self {
        Self {
            name,
            values: Values::Int(min, max),
        }
    }

    /// The property name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Why the property cannot be registered, if it cannot.
    fn check(&self) -> Option<&'static str> {
        if !is_name(self.name) {
            return Some("a property name is empty, too long or not of a-z, 0-9, `_`, `.`, `:`");
        }
        let count = self.count();
        if count == 0 || count > Self::MAX_VALUES {
            return Some("a property has no values or more than 16");
        }
        if let Values::Text(words) = self.values {
            if words
                .iter()
                .any(|w| w.is_empty() || w.len() > Self::MAX_NAME_BYTES)
            {
                return Some("a property value is empty or longer than 64 bytes");
            }
            if words
                .iter()
                .enumerate()
                .any(|(i, w)| words.get(..i).is_some_and(|before| before.contains(w)))
            {
                return Some("a property value is given twice");
            }
        }
        None
    }

    /// How many values the property has (0 if its range is empty, which registering refuses).
    fn count(&self) -> u32 {
        match self.values {
            Values::Bool => 2,
            Values::Int(min, max) => {
                u32::try_from(i64::from(max) - i64::from(min) + 1).unwrap_or(0)
            }
            Values::Text(words) => u32::try_from(words.len()).unwrap_or(u32::MAX),
        }
    }

    /// The `{name, enum}` compound `StartGame` lists the property with.
    fn nbt(&self) -> Compound {
        let values = match self.values {
            Values::Bool => List::Byte(vec![0, 1]),
            Values::Int(min, max) => List::Int((min..=max).collect()),
            Values::Text(words) => List::String(words.iter().map(|w| (*w).to_owned()).collect()),
        };
        Compound::from([
            ("enum".to_owned(), Value::List(values)),
            ("name".to_owned(), Value::String(self.name.to_owned())),
        ])
    }

    /// The place of `value` among the values, if it is one of them.
    fn position(&self, value: PropertyValue<'_>) -> Option<u32> {
        match (self.values, value) {
            (Values::Bool, PropertyValue::Byte(b)) if b <= 1 => Some(u32::from(b)),
            (Values::Int(min, max), PropertyValue::Int(v)) if (min..=max).contains(&v) => {
                u32::try_from(i64::from(v) - i64::from(min)).ok()
            }
            (Values::Text(words), PropertyValue::String(s)) => words
                .iter()
                .position(|w| *w == s)
                .and_then(|p| u32::try_from(p).ok()),
            _ => None,
        }
    }

    /// The value at `at` (0 is the default).
    fn value(&self, at: u32) -> Option<PropertyValue<'static>> {
        match self.values {
            Values::Bool => match at {
                0 => Some(PropertyValue::Byte(0)),
                1 => Some(PropertyValue::Byte(1)),
                _ => None,
            },
            Values::Int(min, max) => {
                let v = i64::from(min).checked_add(i64::from(at))?;
                i32::try_from(v)
                    .ok()
                    .filter(|v| *v <= max)
                    .map(PropertyValue::Int)
            }
            Values::Text(words) => words
                .get(usize::try_from(at).ok()?)
                .map(|w| PropertyValue::String(w)),
        }
    }
}

/// The values of a custom block property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Values {
    /// False and true, sent as a byte 0 or 1.
    Bool,
    /// Every whole number from the first to the second, sent as an int.
    Int(i32, i32),
    /// These words, sent as strings.
    Text(&'static [&'static str]),
}

/// What a face of a custom block shows: a texture of the resource pack (its short name in
/// `terrain_texture.json`) and how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Material {
    /// `*` (every face) or a face: `up`, `down`, `north`, `south`, `east`, `west`.
    target: &'static str,
    texture: &'static str,
    render_method: RenderMethod,
    face_dimming: bool,
}

impl Material {
    /// `texture` on every face, opaque, dimmed by the face's direction.
    pub const fn new(texture: &'static str) -> Self {
        Self {
            target: "*",
            texture,
            render_method: RenderMethod::Opaque,
            face_dimming: true,
        }
    }

    /// `texture` on one face (it takes the place of the every-face material there).
    pub const fn face(face: block::Face, texture: &'static str) -> Self {
        let target = match face {
            block::Face::Down => "down",
            block::Face::Up => "up",
            block::Face::North => "north",
            block::Face::South => "south",
            block::Face::West => "west",
            block::Face::East => "east",
        };
        Self {
            target,
            ..Self::new(texture)
        }
    }

    /// The same material drawn with `method`.
    #[must_use]
    pub const fn with_render_method(mut self, method: RenderMethod) -> Self {
        self.render_method = method;
        self
    }

    /// The same material without dimming by direction.
    #[must_use]
    pub const fn without_face_dimming(mut self) -> Self {
        self.face_dimming = false;
        self
    }

    /// The `material_instances` entry: ambient occlusion only for the methods that keep it.
    fn nbt(&self) -> Compound {
        let occlusion = match self.render_method {
            RenderMethod::Opaque | RenderMethod::DoubleSided => 1.0,
            RenderMethod::AlphaTest | RenderMethod::Blend => 0.0,
        };
        Compound::from([
            ("ambient_occlusion".to_owned(), Value::Float(occlusion)),
            (
                "face_dimming".to_owned(),
                Value::Byte(u8::from(self.face_dimming)),
            ),
            (
                "render_method".to_owned(),
                Value::String(self.render_method.name().to_owned()),
            ),
            ("texture".to_owned(), Value::String(self.texture.to_owned())),
        ])
    }
}

/// How a material is drawn (Bedrock `render_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenderMethod {
    /// Fully opaque textures.
    Opaque,
    /// Pixels fully opaque or fully transparent, such as glass; no ambient occlusion.
    AlphaTest,
    /// Translucent pixels, such as stained glass; no ambient occlusion.
    Blend,
    /// Opaque, drawn from both sides (no back-face culling).
    DoubleSided,
}

impl RenderMethod {
    /// The `render_method` name.
    const fn name(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::AlphaTest => "alpha_test",
            Self::Blend => "blend",
            Self::DoubleSided => "double_sided",
        }
    }
}

/// What breaking a custom block drops.
#[derive(Debug, Clone, Copy)]
pub enum Drops {
    /// One of its own item (the default).
    Itself,
    /// Nothing.
    Nothing,
    /// These items and counts, with a compatible tool.
    Items(&'static [(ItemRef, u8)]),
}

/// What a custom block does when players use it, place it or break it, and on random ticks: the
/// extension point for custom block behavior. Each hook runs in the tick's serial step with the
/// whole game, once the engine has applied the action (its listeners first; the held item, drops
/// and wear included) and before the action's monitors; direct API calls in it raise no events.
pub trait Behavior: Send + Sync {
    /// The block at `pos` was broken and removed, by `player` if one broke it. Its drops have
    /// been spawned and the tool's wear applied. An explosion destroying the block runs the hook
    /// after the explosion; an explosion the hook starts is nested, and past a few levels it does
    /// nothing.
    fn on_break(
        &self,
        cx: &mut Context<'_>,
        player: Option<player::Id>,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) {
        let _ = (cx, player, world, pos, state);
    }

    /// The block was placed at `pos` by `player`, whose held item is already used up. Only a
    /// player's placing runs it: a block set through the API does not.
    fn on_place(
        &self,
        cx: &mut Context<'_>,
        player: player::Id,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) {
        let _ = (cx, player, world, pos, state);
    }

    /// A random tick reached the block at `pos`. Only a block whose definition takes random ticks
    /// ([`Block::random_ticks`]) gets them: each tick, at a few random positions of every
    /// sub-chunk within `tick-radius` of a player.
    fn on_random_tick(
        &self,
        cx: &mut Context<'_>,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) {
        let _ = (cx, world, pos, state);
    }

    /// The player used (right-clicked) the block at `pos`. True if the block did something: then
    /// the held item is neither used nor placed. False means nothing was done: the engine goes
    /// on to use the held item or place it, checking the click again first. A client holding the
    /// button repeats the click, and the hook runs again at least 2 ticks after its last run.
    fn on_use(
        &self,
        cx: &mut Context<'_>,
        player: player::Id,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) -> bool {
        let _ = (cx, player, world, pos, state);
        false
    }
}

/// The client's shape of a custom block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Geometry {
    /// A full cube (`minecraft:geometry.full_block`).
    FullCube,
    /// A model of the resource pack, such as `geometry.lamp`.
    Model(&'static str),
}

/// The `minecraft:collision_box` component of a box.
fn collision_box(b: Aabb) -> Compound {
    let corner = Compound::from([
        ("maxX".to_owned(), Value::Float(pixels(b.max.x))),
        ("maxY".to_owned(), Value::Float(pixels(b.max.y))),
        ("maxZ".to_owned(), Value::Float(pixels(b.max.z))),
        ("minX".to_owned(), Value::Float(pixels(b.min.x))),
        ("minY".to_owned(), Value::Float(pixels(b.min.y))),
        ("minZ".to_owned(), Value::Float(pixels(b.min.z))),
    ]);
    Compound::from([
        (
            "boxes".to_owned(),
            Value::List(List::Compound(vec![corner])),
        ),
        ("enabled".to_owned(), Value::Byte(1)),
    ])
}

/// The `minecraft:selection_box` component of a box (its origin is from the bottom center of the
/// cell).
fn selection_box(b: Aabb) -> Compound {
    let half_cell = to_f32(PIXELS_PER_BLOCK / 2.0);
    let origin = vec![
        pixels(b.min.x) - half_cell,
        pixels(b.min.y),
        pixels(b.min.z) - half_cell,
    ];
    let size = vec![
        pixels(b.max.x - b.min.x),
        pixels(b.max.y - b.min.y),
        pixels(b.max.z - b.min.z),
    ];
    Compound::from([
        ("enabled".to_owned(), Value::Byte(1)),
        ("origin".to_owned(), Value::List(List::Float(origin))),
        ("size".to_owned(), Value::List(List::Float(size))),
    ])
}

/// A coordinate in pixels ([`PIXELS_PER_BLOCK`] a block), as the box components take them.
fn pixels(v: f64) -> f32 {
    to_f32(v * PIXELS_PER_BLOCK)
}

/// Whether `b` is the whole cell.
const fn is_full(b: Aabb) -> bool {
    b.min.x == 0.0
        && b.min.y == 0.0
        && b.min.z == 0.0
        && b.max.x == 1.0
        && b.max.y == 1.0
        && b.max.z == 1.0
}

/// Whether `s` is a property name: 1 to [`Property::MAX_NAME_BYTES`] bytes of `a-z`, `0-9`, `_`,
/// `.`, `:`.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= Property::MAX_NAME_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 × 16 × 16 × 2 states, each property within its limits.
    static TOO_MANY_STATES: [Property; 4] = [
        Property::int("a", 0, 15),
        Property::int("b", 0, 15),
        Property::int("c", 0, 15),
        Property::bool("d"),
    ];

    /// An every-face material and two for the top.
    static TWICE: [Material; 3] = [
        Material::new("a"),
        Material::face(block::Face::Up, "b"),
        Material::face(block::Face::Up, "c"),
    ];

    #[test]
    fn the_refusals_name_the_limits_they_enforce() {
        let max_values = i32::try_from(Property::MAX_VALUES).unwrap();
        assert_eq!(
            Property::int("p", 0, max_values).check(),
            Some(
                format!(
                    "a property has no values or more than {}",
                    Property::MAX_VALUES
                )
                .as_str()
            )
        );
        let long: &'static str = String::leak("x".repeat(Property::MAX_NAME_BYTES + 1));
        assert_eq!(
            Property::enumeration("p", Vec::leak(vec![long])).check(),
            Some(
                format!(
                    "a property value is empty or longer than {} bytes",
                    Property::MAX_NAME_BYTES
                )
                .as_str()
            )
        );
        let many = Vec::leak(vec![Property::bool("p"); Block::MAX_PROPERTIES + 1]);
        assert_eq!(
            Block::new("a:b", "t").with_properties(many).check(),
            Some(format!("more than {} properties", Block::MAX_PROPERTIES).as_str())
        );
        let states = format!(
            "{},{:03}",
            Block::MAX_STATES / 1000,
            Block::MAX_STATES % 1000
        );
        assert_eq!(
            Block::new("a:b", "t")
                .with_properties(&TOO_MANY_STATES)
                .check(),
            Some(format!("more than {states} states").as_str())
        );
        assert_eq!(
            Block::new("a:b", "t")
                .with_light_emission(Block::MAX_LIGHT_LEVEL + 1)
                .check(),
            Some(format!("a light level above {}", Block::MAX_LIGHT_LEVEL).as_str())
        );
        assert_eq!(
            Block::new("a:b", "t")
                .with_tool(ToolType::PICKAXE, Block::MAX_HARVEST_LEVEL + 1)
                .check(),
            Some(
                format!(
                    "a harvest level above {} (netherite)",
                    Block::MAX_HARVEST_LEVEL
                )
                .as_str()
            )
        );
        let drops = Vec::leak(vec![
            (ItemRef::Block(block::Kind::Dirt), 1);
            Block::MAX_DROPS + 1
        ]);
        assert_eq!(
            Block::new("a:b", "t")
                .with_drops(Drops::Items(drops))
                .check(),
            Some(format!("more than {} drops, or a drop count of 0", Block::MAX_DROPS).as_str())
        );
    }

    #[test]
    fn a_friction_the_client_cannot_slide_is_refused() {
        let refusal = Some("a friction outside 0.1 to 1");
        let with = |friction| Block::new("a:b", "t").with_friction(friction).check();
        assert_eq!(with(Block::MIN_FRICTION - 0.05), refusal);
        assert_eq!(with(1.5), refusal);
        assert_eq!(with(f64::NAN), refusal);
        assert_eq!(with(Block::MIN_FRICTION), None);
        assert_eq!(with(1.0), None);
    }

    #[test]
    fn a_face_with_two_materials_is_refused() {
        assert_eq!(
            Block::new("a:b", "t").with_materials(&TWICE).check(),
            Some("a face has two materials")
        );
        assert_eq!(
            Block::new("a:b", "t").with_materials(&TWICE[..2]).check(),
            None
        );
    }
}
