//! Tests of custom content: registration and its checks, the representation of custom types and
//! states, the start-game data and stacks.

use super::*;
use crate::block::{self, PropertyValue, ToolType};
use crate::item::{self, Stack};
use crate::math::{Aabb, Vec3};
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::CreativeCategory;
use crate::protocol::packet::Packet;
use crate::world::{Chunk, Entry};

/// A block that drops air.
static AIR_DROP: Block = Block::new("test:air_drop", "x")
    .with_drops(Drops::Items(&[(ItemRef::Block(block::Kind::Air), 1)]));
/// An item that is always edible, with a glint and a creative group.
pub(crate) static APPLE: Item = Item::new("test:apple", "Golden Thing", "test_apple")
    .with_food_always(4, 9.6)
    .glint()
    .with_creative(Creative::new(Category::Items).with_group("itemGroup.name.test"));
/// A block whose one state hashes, as found by a search, to the network ID of a reference state
/// that is not a state of the table: an alias's.
static AS_ALIAS: Block = Block::new("test:c26250", "x");
/// A block whose one state hashes, as found by a search, to the network ID of a state of the
/// table that the dump has no type for.
static AS_STATE: Block = Block::new("test:c257583", "x");
/// An item whose stack size is past the most a stack can hold.
static BIG_STACK: Item = Item::new("test:big", "Big", "big").with_max_stack_size(65);
/// A block whose light emission is past the most a block can emit.
static BRIGHT: Block = Block::new("test:bright", "x").with_light_emission(16);
/// A block that tells the client it burns.
static FLAMMABLE: Block = Block::new("test:flammable", "x").with_flammability(5, 20);
/// A transparent block with three properties, a geometry, a collision box and materials.
pub(crate) static LAMP: Block = Block::new("test:lamp", "test_lamp")
    .with_properties(&[
        Property::enumeration("color", &["red", "blue"]),
        Property::int("level", 0, 3),
        Property::bool("lit"),
    ])
    .with_light_emission(15)
    .transparent()
    .with_geometry("geometry.test_lamp")
    .with_collision_box(Some(Aabb::new(
        Vec3::new(0.25, 0.0, 0.25),
        Vec3::new(0.75, 0.5, 0.75),
    )))
    .with_materials(&[
        Material::new("test_lamp").with_render_method(RenderMethod::AlphaTest),
        Material::face(block::Face::Up, "test_lamp_top"),
    ]);
/// An item in the `minecraft` namespace.
static MINECRAFT: Item = Item::new("minecraft:ruby", "Ruby", "ruby");
/// An item whose identifier has no namespace.
static NO_NAMESPACE: Item = Item::new("ruby", "Ruby", "ruby");
/// A block that drops two rubies.
pub(crate) static ORE: Block = Block::new("test:ruby_ore", "test_ruby_ore")
    .with_hardness(3.0)
    .with_tool(ToolType::PICKAXE, 2)
    .with_drops(Drops::Items(&[(ItemRef::Custom(&RUBY), 2)]))
    .with_creative(Creative::new(Category::Nature));
/// A block that drops an item nothing registers.
static ORPHAN_DROP: Block =
    Block::new("test:orphan", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&RUBY), 1)]));
/// A block whose collision box leaves the block's cell.
static OUTSIDE: Block = Block::new("test:outside", "x").with_collision_box(Some(Aabb::new(
    Vec3::new(0.0, 0.0, 0.0),
    Vec3::new(1.0, 1.5, 1.0),
)));
/// An item with a creative group.
pub(crate) static RUBY: Item = Item::new("test:ruby", "Ruby", "test_ruby")
    .with_creative(Creative::new(Category::Items).with_group("itemGroup.name.test"));
/// A tool item with durability and attack damage, held like a tool.
pub(crate) static SWORD: Item = Item::new("test:sword", "Ruby Sword", "test_sword")
    .with_durability(100)
    .with_attack_damage(7)
    .hand_equipped()
    .off_hand_allowed()
    .without_creative_breaking();
/// A block that drops one sword.
static SWORD_DROP: Block =
    Block::new("test:one_sword", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&SWORD), 1)]));
/// A block that drops two swords.
static SWORDS_DROP: Block =
    Block::new("test:swords", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&SWORD), 2)]));
/// A block whose properties are not in name order.
static UNSORTED: Block = Block::new("test:unsorted", "x")
    .with_properties(&[Property::bool("lit"), Property::int("level", 0, 1)]);
/// An item whose identifier has an uppercase letter.
static UPPER: Item = Item::new("test:Ruby", "Ruby", "ruby");
/// A block with a property past the most values a property can have.
static WIDE: Block = Block::new("test:wide", "x").with_properties(&[Property::int("level", 0, 16)]);
/// An item with durability and a stack size, which a worn stack cannot keep.
static WORN_STACK: Item = Item::new("test:worn", "Worn", "worn")
    .with_durability(5)
    .with_max_stack_size(16);

/// A finished registry of every test definition.
pub(crate) fn registry() -> Registry {
    let mut r = Registry::default();
    r.add_item(&RUBY);
    r.add_block(&ORE);
    r.add_item(&APPLE);
    r.add_block(&LAMP);
    r.add_item(&SWORD);
    r.finish().expect("the test content registers");
    r
}

#[test]
fn runtime_ids_follow_the_vanilla_ones_in_registration_order() {
    let r = registry();
    let first = item::max_runtime_id() + 1;
    assert_eq!(first, 885, "the 1.26.60 list ends at 884");
    let ruby = r.item_type(&RUBY).unwrap();
    let ore = r.block_state(&ORE).unwrap();
    assert_eq!(ruby.runtime_id(), first);
    assert_eq!(
        r.resolve(ItemRef::CustomBlock(&ORE)).unwrap().runtime_id(),
        first + 1
    );
    assert_eq!(r.item_type(&APPLE).unwrap().runtime_id(), first + 2);
    assert_eq!(r.item_type(&SWORD).unwrap().runtime_id(), first + 4);
    assert_eq!(ruby.name(), "test:ruby");
    assert_eq!(ruby.custom_item().map(Item::identifier), Some("test:ruby"));
    assert_eq!(ruby.custom_item().map(Item::display_name), Some("Ruby"));
    assert_eq!(ore.name(), "test:ruby_ore");
    assert_eq!(
        ore.custom_block().map(Block::identifier),
        Some("test:ruby_ore")
    );
    // A block is not an item and the other way round.
    assert!(r.item_type(&SWORD).unwrap().custom_block().is_none());
    let registry = r.item_registry();
    let vanilla = item::registry();
    assert_eq!(registry.items.len(), vanilla.items.len() + 5);
    assert_eq!(registry.items[..vanilla.items.len()], vanilla.items[..]);
    let ruby_entry = &registry.items[vanilla.items.len()];
    assert_eq!(
        (
            ruby_entry.name.as_str(),
            ruby_entry.runtime_id,
            ruby_entry.component_based,
            ruby_entry.version
        ),
        ("test:ruby", first, true, 1)
    );
    let ore_entry = &registry.items[vanilla.items.len() + 1];
    assert_eq!(
        (
            ore_entry.component_based,
            ore_entry.version,
            ore_entry.data.len()
        ),
        (false, 2, 0),
        "a block's item is a plain block item"
    );
}

fn compound<'a>(c: &'a Compound, key: &str) -> &'a Compound {
    match c.get(key) {
        Some(Value::Compound(c)) => c,
        other => panic!("{key}: {other:?}"),
    }
}

#[test]
fn item_components_have_the_shape_the_client_reads() {
    let sword = SWORD.to_components();
    let components = compound(&sword, "components");
    let properties = compound(components, "item_properties");
    assert_eq!(properties.get("max_stack_size"), Some(&Value::Int(1)));
    assert_eq!(properties.get("hand_equipped"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("allow_off_hand"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("damage"), Some(&Value::Int(7)));
    assert_eq!(
        properties.get("can_destroy_in_creative"),
        Some(&Value::Byte(0))
    );
    assert_eq!(
        properties.get("creative_category"),
        Some(&Value::Int(CreativeCategory::Items as i32))
    );
    let icon = compound(compound(properties, "minecraft:icon"), "textures");
    assert_eq!(
        icon.get("default"),
        Some(&Value::String("test_sword".to_owned()))
    );
    assert_eq!(
        compound(components, "minecraft:durability").get("max_durability"),
        Some(&Value::Int(100))
    );
    assert_eq!(
        compound(components, "minecraft:display_name").get("value"),
        Some(&Value::String("Ruby Sword".to_owned()))
    );
    let apple = APPLE.to_components();
    let components = compound(&apple, "components");
    let properties = compound(components, "item_properties");
    assert_eq!(properties.get("foil"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("can_destroy_in_creative"), None);
    assert_eq!(properties.get("use_duration"), Some(&Value::Int(32)));
    assert_eq!(
        properties.get("creative_group"),
        Some(&Value::String("itemGroup.name.test".to_owned()))
    );
    assert_eq!(
        compound(components, "minecraft:food").get("can_always_eat"),
        Some(&Value::Byte(1))
    );
}

#[test]
fn custom_types_answer_the_rules_from_their_definition() {
    let r = registry();
    let sword = r.item_type(&SWORD).unwrap();
    assert_eq!(sword.max_stack_size(), 1);
    assert_eq!(sword.max_durability(), Some(100));
    assert_eq!(sword.attack_points(), 7);
    assert!(sword.is(item::Class::Durable) && sword.is(item::Class::Item));
    assert_eq!(sword.block(), None);
    let apple = r.item_type(&APPLE).unwrap();
    let food = apple.food().unwrap();
    assert_eq!((food.food_restore(), food.saturation_restore()), (4, 9.6));
    assert!(!food.requires_food());
    assert!(apple.consumable().is_some() && apple.is(item::Class::Food));
    let ruby = r.item_type(&RUBY).unwrap();
    assert_eq!(ruby.max_stack_size(), 64);
    assert!(ruby.food().is_none());
    let ore_item = r.resolve(ItemRef::CustomBlock(&ORE)).unwrap();
    assert_eq!(ore_item.class(), item::Class::ItemBlock);
    assert_eq!(ore_item.block(), r.block_state(&ORE));
    assert_eq!(
        ore_item.placed_block(Some(block::Face::Up)),
        r.block_state(&ORE)
    );
    // Equality and order never depend on addresses.
    assert_eq!(ruby, r.item_type(&RUBY).unwrap());
    assert_ne!(ruby, apple);
    assert!(item::Kind::Stick.default_type() < ruby && ruby < apple);
    assert_eq!(format!("{ruby:?}"), "Type { name: \"test:ruby\", meta: 0 }");
}

#[test]
fn custom_states_follow_their_properties() {
    let r = registry();
    let lamp = r.block_state(&LAMP).unwrap();
    let props: Vec<_> = lamp.properties().map(|p| (p.name(), p.value())).collect();
    assert_eq!(
        props,
        [
            ("color", PropertyValue::String("red")),
            ("level", PropertyValue::Int(0)),
            ("lit", PropertyValue::Byte(0)),
        ]
    );
    let lit = lamp.with("lit", PropertyValue::Byte(1)).unwrap();
    let blue = lit
        .with("color", PropertyValue::String("blue"))
        .unwrap()
        .with("level", PropertyValue::Int(2))
        .unwrap();
    assert_eq!(blue.value("level"), Some(PropertyValue::Int(2)));
    assert_eq!(blue.value("lit"), Some(PropertyValue::Byte(1)));
    assert_eq!(lamp.with("level", PropertyValue::Int(4)), None);
    assert_eq!(lamp.with("color", PropertyValue::String("green")), None);
    assert_eq!(lamp.with("nope", PropertyValue::Byte(1)), None);
    assert_eq!(
        blue.with("color", PropertyValue::String("red"))
            .unwrap()
            .with("level", PropertyValue::Int(0))
            .unwrap()
            .with("lit", PropertyValue::Byte(0)),
        Some(lamp)
    );
    // The data the rules read.
    assert_eq!(lamp.light_emission(), 15);
    assert_eq!(lamp.light_filter(), 0, "transparent");
    assert!(lamp.is_transparent() && lamp.is(block::Class::Transparent));
    assert_eq!(lamp.kind(), block::Kind::InfoUpdate);
    assert_eq!(lamp.collision_boxes().len(), 1);
    assert_eq!(lamp.support(block::Face::Up), block::SupportType::None);
    assert!(!lamp.has_trait(block::Trait::Fallable));
    let ore = r.block_state(&ORE).unwrap();
    assert!(ore.is_solid() && !ore.is_transparent() && ore.is(block::Class::Opaque));
    assert_eq!(ore.light_filter(), 15);
    assert!(ore.blocks_direct_sky_light());
    assert_eq!(ore.support(block::Face::Up), block::SupportType::Full);
    let info = ore.break_info();
    assert_eq!((info.hardness(), info.blast_resistance()), (3.0, 15.0));
    assert_eq!(
        (info.tool_type(), info.harvest_level()),
        (ToolType::PICKAXE, 2)
    );
    assert_eq!(ore.properties().len(), 0);
    assert_eq!(
        format!("{blue:?}"),
        "State { name: \"test:lamp\", color: String(\"blue\"), level: Int(2), lit: Byte(1) }"
    );
    // The disk form names the custom block, and reads back through the registry.
    let disk = crate::save::state_nbt(blue);
    assert_eq!(
        disk.get("name"),
        Some(&Value::String("test:lamp".to_owned()))
    );
    assert_eq!(
        crate::save::find_state(&disk),
        None,
        "the table does not know it"
    );
    assert_eq!(r.find_state_nbt(&disk), Some(blue));
    // Every state is distinct, and order and equality never depend on addresses.
    assert!(block::AIR < lamp && lamp < blue && ore > blue);
    match ore.item() {
        block::ItemForm::Item(t) => assert_eq!(t.runtime_id(), item::max_runtime_id() + 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_palette_entry_must_name_exactly_the_properties_of_the_block() {
    let states = |color: Value, level: Value, lit: Value| {
        Compound::from([
            ("color".to_owned(), color),
            ("level".to_owned(), level),
            ("lit".to_owned(), lit),
        ])
    };
    let blue = || Value::String("blue".to_owned());
    let index = LAMP
        .index_of(&states(blue(), Value::Int(2), Value::Byte(1)))
        .unwrap();
    assert_eq!(
        LAMP.property_value(index, 0),
        Some(PropertyValue::String("blue"))
    );
    assert_eq!(LAMP.property_value(index, 1), Some(PropertyValue::Int(2)));
    assert_eq!(LAMP.property_value(index, 2), Some(PropertyValue::Byte(1)));
    for wrong in [
        states(
            Value::String("green".to_owned()),
            Value::Int(2),
            Value::Byte(1),
        ),
        states(blue(), Value::Int(4), Value::Byte(1)),
        states(blue(), Value::Int(2), Value::Byte(2)),
        states(blue(), Value::String("2".to_owned()), Value::Byte(1)),
        states(blue(), Value::Int(2), Value::Float(1.0)),
        Compound::from([
            ("color".to_owned(), blue()),
            ("level".to_owned(), Value::Int(2)),
        ]),
        Compound::from([
            ("color".to_owned(), blue()),
            ("extra".to_owned(), Value::Byte(0)),
            ("level".to_owned(), Value::Int(2)),
            ("lit".to_owned(), Value::Byte(1)),
        ]),
        Compound::from([
            ("color".to_owned(), blue()),
            ("level".to_owned(), Value::Int(2)),
            ("lot".to_owned(), Value::Byte(1)),
        ]),
    ] {
        assert_eq!(LAMP.index_of(&wrong), None, "{wrong:?}");
    }
}

/// Custom network IDs are the network block hash of `test:lamp` with {color red, level 0, lit 0}
/// and with {color blue, level 2, lit 1}, and of `test:ruby_ore` without properties. The golden
/// values come from an independent implementation of the hash, run with `go run`.
#[test]
fn custom_network_ids_are_the_network_block_hashes() {
    let r = registry();
    let lamp = r.block_state(&LAMP).unwrap();
    assert_eq!(lamp.network_id(), 0x181a_df90);
    let blue = lamp
        .with("color", PropertyValue::String("blue"))
        .and_then(|s| s.with("level", PropertyValue::Int(2)))
        .and_then(|s| s.with("lit", PropertyValue::Byte(1)))
        .unwrap();
    assert_eq!(blue.network_id(), 0x6f11_ab71);
    assert_eq!(r.block_state(&ORE).unwrap().network_id(), 0x6cd0_c0a0);
    // The block item is sent with its default state.
    let item = r.resolve(ItemRef::CustomBlock(&LAMP)).unwrap();
    assert_eq!(
        Stack::new(item, 1).to_network().block_runtime_id,
        0x181a_df90_u32.cast_signed()
    );
}

#[test]
fn start_game_blocks_carry_the_components_the_client_reads() {
    let r = registry();
    let blocks = r.to_block_entries();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].name, "test:ruby_ore");
    let ore = &blocks[0].properties;
    assert_eq!(
        compound(ore, "vanilla_block_data").get("block_id"),
        Some(&Value::Int(10_000))
    );
    assert_eq!(
        compound(&blocks[1].properties, "vanilla_block_data").get("block_id"),
        Some(&Value::Int(10_001))
    );
    assert_eq!(ore.get("properties"), None);
    assert_eq!(
        compound(ore, "menu_category").get("category"),
        Some(&Value::String("nature".to_owned()))
    );
    let components = compound(ore, "components");
    // 3 × 5 seconds by hand without a pickaxe.
    assert_eq!(
        compound(components, "minecraft:destructible_by_mining").get("value"),
        Some(&Value::Float(15.0))
    );
    assert_eq!(
        compound(components, "minecraft:geometry").get("identifier"),
        Some(&Value::String("minecraft:geometry.full_block".to_owned()))
    );
    let lamp = &blocks[1].properties;
    let Some(Value::List(List::Compound(properties))) = lamp.get("properties") else {
        panic!("{lamp:?}");
    };
    assert_eq!(properties.len(), 3);
    assert_eq!(
        properties[1].get("enum"),
        Some(&Value::List(List::Int(vec![0, 1, 2, 3])))
    );
    assert_eq!(
        properties[2].get("enum"),
        Some(&Value::List(List::Byte(vec![0, 1])))
    );
    let components = compound(lamp, "components");
    let materials = compound(
        compound(components, "minecraft:material_instances"),
        "materials",
    );
    assert_eq!(materials.len(), 2);
    assert_eq!(
        compound(materials, "*").get("render_method"),
        Some(&Value::String("alpha_test".to_owned()))
    );
    assert_eq!(
        compound(materials, "up").get("texture"),
        Some(&Value::String("test_lamp_top".to_owned()))
    );
    let collision = compound(components, "minecraft:collision_box");
    let Some(Value::List(List::Compound(boxes))) = collision.get("boxes") else {
        panic!("{collision:?}");
    };
    assert_eq!(boxes[0].get("maxY"), Some(&Value::Float(8.0)));
    assert_eq!(boxes[0].get("minX"), Some(&Value::Float(4.0)));
    assert_eq!(
        compound(components, "minecraft:light_emission").get("emission"),
        Some(&Value::Int(15))
    );
    // Only a block given a flammability carries the component.
    assert!(!compound(ore, "components").contains_key("minecraft:flammable"));
    let flammable = FLAMMABLE.entry_nbt(10_002);
    let flammable = compound(compound(&flammable, "components"), "minecraft:flammable");
    assert_eq!(flammable.get("burn_odds"), Some(&Value::Int(5)));
    assert_eq!(flammable.get("flame_odds"), Some(&Value::Int(20)));
    // Every definition round-trips through the codec.
    let packet = Packet::StartGame(Box::new(crate::protocol::packet::StartGame {
        blocks,
        ..crate::protocol::packet::StartGame::default()
    }));
    let mut bytes = Vec::new();
    packet.encode(&mut bytes).unwrap();
    let mut budget = usize::MAX;
    assert_eq!(
        Packet::decode(packet.id(), &bytes, &mut budget).unwrap(),
        packet
    );
}

/// The default creative inventory with nothing registered: the vanilla one without the Education
/// items.
fn vanilla_content() -> crate::protocol::packet::CreativeContent {
    creative::Resolved::vanilla().content().clone()
}

/// The group index the grouping rule gives the next entry of `category` without a group after
/// `content`: the category's anonymous group if no named group of that category came after its
/// last anonymous entry, else a new group.
fn next_anonymous(content: &crate::protocol::packet::CreativeContent, category: u8) -> u32 {
    let last = content
        .items
        .iter()
        .rev()
        .map(|i| &content.groups[usize::try_from(i.group_index).unwrap()])
        .zip(content.items.iter().rev())
        .find(|(g, _)| g.category == category);
    match last {
        Some((g, i)) if g.name.is_empty() => i.group_index,
        _ => u32::try_from(content.groups.len()).unwrap(),
    }
}

// api: CreativeCategory, CreativeInventoryCache::buildPacket
#[test]
fn the_creative_inventory_lists_custom_entries_after_the_vanilla_ones() {
    let r = registry();
    let vanilla = vanilla_content();
    let content = r.creative_content();
    let n = vanilla.items.len();
    // The ruby and the apple share their group (same name and icon) though the ore comes
    // between them; the ore joins the anonymous nature group the grouping rule leaves open.
    assert_eq!(content.items.len(), n + 3);
    assert_eq!(content.items[..n], vanilla.items[..]);
    assert_eq!(content.groups[..vanilla.groups.len()], vanilla.groups[..]);
    let (ruby, ore, apple) = (
        &content.items[n],
        &content.items[n + 1],
        &content.items[n + 2],
    );
    assert_eq!(ruby.creative_item_network_id, u32::try_from(n).unwrap() + 1);
    assert_eq!(ruby.group_index, apple.group_index);
    assert_eq!(
        ore.group_index,
        next_anonymous(&vanilla, CreativeCategory::Nature as u8)
    );
    let named = &content.groups[usize::try_from(ruby.group_index).unwrap()];
    assert_eq!(
        (named.category, named.name.as_str()),
        (CreativeCategory::Items as u8, "itemGroup.name.test")
    );
    let mut icon = ruby.item.clone();
    icon.nbt_data.insert(
        item::GROUP_WORKAROUND.to_owned(),
        Value::Int(i32::try_from(ruby.group_index).unwrap()),
    );
    assert_eq!(named.icon, icon, "the first item, with its group index");
    let anonymous = &content.groups[usize::try_from(ore.group_index).unwrap()];
    assert_eq!(
        (anonymous.category, anonymous.name.as_str()),
        (CreativeCategory::Nature as u8, "")
    );
    // The stacks the client takes come back as the custom items.
    let id = ruby.creative_item_network_id;
    assert_eq!(
        r.creative_stack(0, id),
        Some(Stack::new(r.item_type(&RUBY).unwrap(), 1))
    );
    assert_eq!(r.creative_stack(0, 1), item::creative_stack(1));
    assert_eq!(r.creative_stack(0, id + 3), None);
    assert_eq!(r.creative_stack(1, 1), None, "no alternative inventory");
}

/// The finished registry of `builder`.
fn finished(builder: crate::ServerBuilder) -> Result<Registry> {
    let mut r = builder.setup.custom;
    r.finish().map(|()| r)
}

/// The item types of a creative packet's entries, in order.
fn listed(content: &crate::protocol::packet::CreativeContent) -> Vec<i32> {
    content
        .items
        .iter()
        .map(|i| i.item.item_type.network_id)
        .collect()
}

/// The item of the vanilla item type `kind`.
fn vanilla(kind: item::Kind) -> ItemRef {
    ItemRef::Item(kind.default_type())
}

// api: CreativeInventory, CreativeInventory::add, CreativeInventory::remove,
// api: CreativeInventory::clear, CreativeInventoryEntry::matchesItem, CreativeGroup,
// api: CreativeGroup::__construct, CreativeGroup::getName, CreativeGroup::getIcon
#[test]
fn builder_changes_remove_clear_and_add_creative_entries_and_groups() {
    let planks = Registry::default()
        .resolve(ItemRef::Block(block::Kind::OakPlanks))
        .unwrap();
    let sword = vanilla(item::Kind::DiamondSword);
    let iron = vanilla(item::Kind::IronSword);
    let apple = vanilla(item::Kind::Apple);
    let book = Stack::new(item::Kind::EnchantedBook.default_type(), 3)
        .with_extra(Some(item::Extra {
            enchantments: vec![(item::Enchantment::Sharpness, 2)],
            ..item::Extra::default()
        }))
        .unwrap();
    let swords = CreativeGroup::new("Swords", sword);
    assert_eq!(swords.name(), "Swords");
    assert!(matches!(
        swords.icon(),
        CreativeItem::Item(ItemRef::Item(t)) if *t == item::Kind::DiamondSword.default_type()
    ));
    let damaged = Stack::new(item::Kind::DiamondSword.default_type(), 1)
        .with_damage(5)
        .unwrap();
    let r = finished(
        crate::Server::builder()
            .custom_item(&RUBY)
            .creative_inventory(
                CreativeInventory::vanilla()
                    .remove(ItemRef::Item(planks))
                    // No entry has this damage, so nothing is removed.
                    .remove(damaged),
            )
            .alternative_creative_inventory(
                "kits",
                CreativeInventory::vanilla()
                    .clear()
                    .add(sword, Category::Equipment, Some(swords.clone()))
                    .add(apple, Category::Items, None)
                    .add(iron, Category::Equipment, Some(swords.clone()))
                    .add(book.clone(), Category::Items, None)
                    .add(ItemRef::Custom(&RUBY), Category::Items, None)
                    .remove(apple),
            ),
    )
    .unwrap();
    let base = vanilla_content();
    // The default: the vanilla list without its first oak planks, then the ruby's place.
    let default = r.creative_content();
    let planks_id = i32::from(planks.runtime_id());
    let mut want = listed(&base);
    let at = want.iter().position(|id| *id == planks_id).unwrap();
    want.remove(at);
    want.push(i32::from(r.item_type(&RUBY).unwrap().runtime_id()));
    assert_eq!(listed(&default), want);
    assert_eq!(
        default
            .items
            .iter()
            .map(|i| i.creative_item_network_id)
            .collect::<Vec<_>>(),
        (1..=u32::try_from(want.len()).unwrap()).collect::<Vec<_>>(),
        "IDs count from 1 without holes"
    );
    assert_eq!(r.creative_stack(0, 1), item::creative_stack(2));
    // The alternative: the swords in one group though the apple was between them, the book and
    // the ruby in the anonymous group that the swords' group did not end (another category).
    let kits = r.creatives()[1].content();
    let ruby = r.item_type(&RUBY).unwrap();
    assert_eq!(
        listed(kits),
        [
            sword,
            iron,
            ItemRef::Item(book.item().unwrap()),
            ItemRef::Custom(&RUBY)
        ]
        .map(|s| i32::from(r.resolve(s).unwrap().runtime_id()))
    );
    let groups: Vec<u32> = kits.items.iter().map(|i| i.group_index).collect();
    assert_eq!(groups, [0, 0, 1, 1]);
    assert_eq!(kits.groups.len(), 2);
    assert_eq!(
        (kits.groups[0].category, kits.groups[0].name.as_str()),
        (CreativeCategory::Equipment as u8, "Swords")
    );
    assert_eq!(
        kits.groups[0].icon.nbt_data.get(item::GROUP_WORKAROUND),
        Some(&Value::Int(0))
    );
    assert_eq!(
        (kits.groups[1].category, kits.groups[1].name.as_str()),
        (CreativeCategory::Items as u8, "")
    );
    assert_eq!(
        kits.items[2].item,
        book.with_count(1).to_network(),
        "the stack's NBT"
    );
    assert_eq!(r.creative_stack(1, 3), Some(book.with_count(1)));
    assert_eq!(r.creative_stack(1, 4), Some(Stack::new(ruby, 1)));
    assert_eq!(r.creative_stack(1, 5), None);
    assert_eq!(r.creative_index(Some("kits")), Some(1));
    assert_eq!(r.creative_index(None), Some(0));
    assert_eq!(r.creative_index(Some("other")), None);
    assert_eq!(r.creative_name(1), Some("kits"));
    assert_eq!(r.creative_name(0), None);
}

#[test]
fn cleared_creative_inventories_are_empty_and_bad_ones_are_refused() {
    let empty =
        finished(crate::Server::builder().creative_inventory(CreativeInventory::vanilla().clear()))
            .unwrap();
    assert!(empty.creative_content().items.is_empty());
    assert!(empty.creative_content().groups.is_empty());
    let apple = vanilla(item::Kind::Apple);
    let refused = |builder: crate::ServerBuilder| finished(builder).unwrap_err();
    assert!(matches!(
        refused(
            crate::Server::builder().creative_inventory(CreativeInventory::empty().add(
                ItemRef::Custom(&RUBY),
                Category::Items,
                None
            ))
        ),
        Error::Creative { .. }
    ));
    assert!(matches!(
        refused(
            crate::Server::builder().creative_inventory(CreativeInventory::empty().add(
                apple,
                Category::Items,
                Some(CreativeGroup::new("", apple))
            ))
        ),
        Error::Creative { .. }
    ));
    assert!(matches!(
        refused(
            crate::Server::builder().creative_inventory(CreativeInventory::empty().add(
                Stack::EMPTY,
                Category::Items,
                None
            ))
        ),
        Error::Creative { .. }
    ));
    assert_eq!(
        refused(
            crate::Server::builder()
                .alternative_creative_inventory("a", CreativeInventory::empty())
                .alternative_creative_inventory("a", CreativeInventory::vanilla())
        ),
        Error::Duplicate { identifier: "a" }
    );
    assert!(matches!(
        refused(
            crate::Server::builder().alternative_creative_inventory("", CreativeInventory::empty())
        ),
        Error::Creative { .. }
    ));
    let mut many = crate::Server::builder();
    let names: Vec<&'static str> = (0..=creative::MAX_ALTERNATIVES)
        .map(|i| &*Box::leak(i.to_string().into_boxed_str()))
        .collect();
    for name in names {
        many = many.alternative_creative_inventory(name, CreativeInventory::empty());
    }
    assert!(matches!(refused(many), Error::TooMany { .. }));
    let mut big = CreativeInventory::empty();
    for _ in 0..=creative::MAX_ENTRIES {
        big = big.add(apple, Category::Items, None);
    }
    assert!(matches!(
        refused(crate::Server::builder().creative_inventory(big)),
        Error::TooMany { .. }
    ));
}

#[test]
fn custom_stacks_round_trip_through_disk_and_network() {
    let r = registry();
    let sword = Stack::new(r.item_type(&SWORD).unwrap(), 1)
        .with_damage(30)
        .unwrap();
    let lamp = Stack::new(r.resolve(ItemRef::CustomBlock(&LAMP)).unwrap(), 9);
    let ruby = Stack::new(r.item_type(&RUBY).unwrap(), 64);
    for stack in [&sword, &lamp, &ruby] {
        let disk = stack.to_nbt();
        assert_eq!(
            Stack::from_nbt_with(&disk, &r).as_ref(),
            Some(stack),
            "{disk:?}"
        );
        assert_eq!(Stack::from_nbt(&disk), None, "the table does not know it");
        let net = stack.to_network();
        assert_eq!(
            i32::from(stack.item().unwrap().runtime_id()),
            net.item_type.network_id
        );
    }
    assert_eq!(
        sword.to_nbt().get("tag"),
        Some(&Value::Compound(Compound::from([(
            "Damage".to_owned(),
            Value::Int(30)
        )])))
    );
    assert!(lamp.to_nbt().contains_key("Block"));
    // Vanilla stacks read as before.
    let stick = Stack::new(item::Kind::Stick.default_type(), 3);
    assert_eq!(Stack::from_nbt_with(&stick.to_nbt(), &r), Some(stick));
    // An unregistered custom item stays unknown.
    assert_eq!(
        Stack::from_nbt_with(&ruby.to_nbt(), &Registry::default()),
        None
    );
}

fn finish(build: impl FnOnce(&mut Registry)) -> Result<()> {
    let mut r = Registry::default();
    build(&mut r);
    r.finish()
}

#[test]
fn a_state_with_the_network_id_of_any_vanilla_state_refuses_the_build() {
    for block in [&AS_STATE, &AS_ALIAS] {
        assert_eq!(
            finish(|r| r.add_block(block)),
            Err(Error::HashCollision {
                identifier: block.identifier()
            })
        );
    }
}

#[test]
fn invalid_definitions_refuse_the_build() {
    assert_eq!(
        finish(|r| r.add_item(&MINECRAFT)),
        Err(Error::Identifier {
            identifier: "minecraft:ruby"
        })
    );
    assert!(matches!(
        finish(|r| r.add_item(&NO_NAMESPACE)),
        Err(Error::Identifier { .. })
    ));
    assert!(matches!(
        finish(|r| r.add_item(&UPPER)),
        Err(Error::Identifier { .. })
    ));
    assert_eq!(
        finish(|r| {
            r.add_item(&RUBY);
            r.add_item(&RUBY);
        }),
        Err(Error::Duplicate {
            identifier: "test:ruby"
        })
    );
    for (item, what) in [(&BIG_STACK, "stack size"), (&WORN_STACK, "durability")] {
        assert!(
            matches!(finish(|r| r.add_item(item)), Err(Error::Definition { .. })),
            "{what}"
        );
    }
    for block in [&UNSORTED, &WIDE, &BRIGHT, &OUTSIDE] {
        let e = finish(|r| r.add_block(block)).unwrap_err();
        assert!(matches!(e, Error::Definition { .. }), "{e}");
    }
    assert_eq!(
        finish(|r| r.add_block(&ORPHAN_DROP)),
        Err(Error::Unregistered {
            identifier: "test:orphan"
        })
    );
    assert_eq!(
        finish(|r| {
            r.add_item(&RUBY);
            r.add_block(&ORPHAN_DROP);
        }),
        Ok(())
    );
    // Two swords do not fit one stack: refused at build, not cut to one when dropped.
    assert_eq!(
        finish(|r| {
            r.add_item(&SWORD);
            r.add_block(&SWORDS_DROP);
        }),
        Err(Error::Definition {
            identifier: "test:swords",
            reason: "a drop count outside its item's stack size",
        })
    );
    assert_eq!(
        finish(|r| {
            r.add_item(&SWORD);
            r.add_block(&SWORD_DROP);
        }),
        Ok(())
    );
    let e = Error::Definition {
        identifier: "test:x",
        reason: "a light level above 15",
    };
    assert_eq!(e.to_string(), "`test:x`: a light level above 15");
}

#[test]
fn drops_naming_no_item_say_so() {
    let e = finish(|r| r.add_block(&AIR_DROP)).unwrap_err();
    assert_eq!(
        e.to_string(),
        "`test:air_drop` drops an unknown or unregistered item"
    );
}

/// No Education item in the creative inventory.
#[test]
fn the_creative_inventory_leaves_the_education_items_out() {
    let names: std::collections::BTreeMap<i32, String> = item::registry()
        .items
        .into_iter()
        .map(|e| (i32::from(e.runtime_id), e.name))
        .collect();
    let is_education = |s: &crate::protocol::ItemStack| {
        names
            .get(&s.item_type.network_id)
            .is_some_and(|n| item::is_education(n))
    };
    let education = |content: &crate::protocol::packet::CreativeContent| {
        content
            .items
            .iter()
            .filter(|c| is_education(&c.item))
            .count()
    };
    let mut r = Registry::default();
    r.finish().unwrap();
    let full = item::creative_content();
    assert!(
        education(&full) > 100,
        "elements, compounds, hardened glass"
    );
    assert_eq!(education(&r.creative_content()), 0);
    assert_eq!(
        r.creative_content().items.len(),
        full.items.len() - education(&full)
    );
}

#[test]
fn nothing_registered_sends_exactly_the_vanilla_content() {
    let mut r = Registry::default();
    r.finish().unwrap();
    assert_eq!(r.item_registry(), item::registry());
    assert_eq!(r.creative_content(), vanilla_content());
    assert!(r.to_block_entries().is_empty());
    let settings = crate::protocol::batch::Settings::default();
    let none = crate::tick::Customs::default();
    let plain = format!(
        "{:?}",
        crate::tick::Shared::new(settings, &crate::custom::Registry::default(), &none).unwrap()
    );
    let custom = format!(
        "{:?}",
        crate::tick::Shared::new(settings, &r, &none).unwrap()
    );
    assert_eq!(plain, custom);
}

/// A saved chunk keeps a custom block by name and state: loaded with the block registered it
/// is the custom state again; without it, the block stays unknown (shown as
/// `minecraft:info_update`) and is saved back exactly as it was.
#[test]
fn saved_custom_blocks_load_back_or_stay_as_they_were_saved() {
    let r = registry();
    let blue = r
        .block_state(&LAMP)
        .and_then(|s| s.with("color", PropertyValue::String("blue")))
        .unwrap();
    let mut chunk = Chunk::new();
    chunk.put(1, -60, 2, Entry::Known(blue)).unwrap();
    let record = crate::save::to_record(&chunk);
    let saved = crate::save::to_chunk(record.clone(), &r).unwrap();
    assert_eq!(saved.block(1, -60, 2), Some(blue));
    assert_eq!(crate::save::to_record(&saved), record);
    // Without the registration.
    let bare = crate::save::to_chunk(record.clone(), &Registry::default()).unwrap();
    assert!(
        matches!(bare.entry(1, -60, 2), Some(Entry::Unknown(_))),
        "an unregistered custom block stays unknown"
    );
    assert_eq!(bare.block(1, -60, 2), None);
    assert_eq!(crate::save::to_record(&bare), record);
}
