//! Tests of custom content: registration and its checks, the representation of custom types and
//! states, the start-game data, recipes and stacks.

use super::*;
use crate::block::{self, PropertyValue, ToolType};
use crate::crafting::Recipe as Judged;
use crate::item::{self, Stack};
use crate::math::{Aabb, Vec3};
use crate::nbt::{Compound, List, Value};
use crate::protocol::consts::CreativeCategory;
use crate::protocol::packet::Packet;

pub(crate) static RUBY: Item = Item::new("test:ruby", "Ruby", "test_ruby")
    .with_creative(Creative::new(Category::Items).with_group("itemGroup.name.test"));
pub(crate) static APPLE: Item = Item::new("test:apple", "Golden Thing", "test_apple")
    .with_food_always(4, 9.6)
    .glint()
    .with_creative(Creative::new(Category::Items).with_group("itemGroup.name.test"));
pub(crate) static SWORD: Item = Item::new("test:sword", "Ruby Sword", "test_sword")
    .with_durability(100)
    .with_attack_damage(7)
    .hand_equipped()
    .allow_off_hand();
pub(crate) static ORE: Block = Block::new("test:ruby_ore", "test_ruby_ore")
    .with_hardness(3.0)
    .with_tool(ToolType::PICKAXE, 2)
    .with_drops(Drops::Items(&[(ItemRef::Custom(&RUBY), 2)]))
    .with_experience(1, 3)
    .with_creative(Creative::new(Category::Nature));
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

/// A registry of every test definition and two recipes, finished.
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

fn vanilla(kind: item::Kind) -> ItemRef {
    ItemRef::Item(kind.default_type())
}

#[test]
fn runtime_ids_follow_the_vanilla_ones_in_registration_order() {
    let r = registry();
    let first = item::max_runtime_id() + 1;
    assert_eq!(first, 884, "the 1.26.50 list ends at 883");
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
fn item_components_have_dragonflys_shape() {
    let sword = SWORD.components();
    let components = compound(&sword, "components");
    let properties = compound(components, "item_properties");
    assert_eq!(properties.get("max_stack_size"), Some(&Value::Int(1)));
    assert_eq!(properties.get("hand_equipped"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("allow_off_hand"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("damage"), Some(&Value::Int(7)));
    assert_eq!(
        properties.get("creative_category"),
        Some(&Value::Int(CreativeCategory::Items as i32))
    );
    let icon = compound(compound(properties, "minecraft:icon"), "textures");
    assert_eq!(
        icon.get("default"),
        Some(&Value::String("test_sword".into()))
    );
    assert_eq!(
        compound(components, "minecraft:durability").get("max_durability"),
        Some(&Value::Int(100))
    );
    assert_eq!(
        compound(components, "minecraft:display_name").get("value"),
        Some(&Value::String("Ruby Sword".into()))
    );
    let apple = APPLE.components();
    let components = compound(&apple, "components");
    let properties = compound(components, "item_properties");
    assert_eq!(properties.get("foil"), Some(&Value::Byte(1)));
    assert_eq!(properties.get("use_duration"), Some(&Value::Int(32)));
    assert_eq!(
        properties.get("creative_group"),
        Some(&Value::String("itemGroup.name.test".into()))
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
    assert!(!food.requires_hunger());
    assert!(apple.consumable().is_some() && apple.is(item::Class::Food));
    assert!(apple.has_trait(item::Trait::Releasable));
    let ruby = r.item_type(&RUBY).unwrap();
    assert_eq!(ruby.max_stack_size(), 64);
    assert!(ruby.food().is_none());
    assert!(!item::Tag::find("minecraft:logs").unwrap().contains(ruby));
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
    assert!(!lamp.ticks_randomly() && !lamp.has_trait(block::Trait::Fallable));
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
    assert!(ore.is_affected_by_silk_touch());
    assert_eq!(ore.properties().len(), 0);
    assert_eq!(
        format!("{blue:?}"),
        "State { name: \"test:lamp\", color: String(\"blue\"), level: Int(2), lit: Byte(1) }"
    );
    // The disk form names the custom block, and reads back through the registry.
    let disk = crate::save::state_nbt(blue);
    assert_eq!(disk.get("name"), Some(&Value::String("test:lamp".into())));
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

/// Golden network IDs from dragonfly v0.11.5's `networkBlockHash` (`go run` over
/// `server/world/network_block_hash.go`): `test:lamp` with {color red, level 0, lit 0} and with
/// {color blue, level 2, lit 1}, and `test:ruby_ore` without properties.
#[test]
fn custom_network_ids_are_dragonflys_hashes() {
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
fn start_game_blocks_carry_dragonflys_components() {
    let r = registry();
    let blocks = r.block_entries();
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
        Some(&Value::String("nature".into()))
    );
    let components = compound(ore, "components");
    // 3 × 5 seconds by hand without a pickaxe.
    assert_eq!(
        compound(components, "minecraft:destructible_by_mining").get("value"),
        Some(&Value::Float(15.0))
    );
    assert_eq!(
        compound(components, "minecraft:geometry").get("identifier"),
        Some(&Value::String("minecraft:geometry.full_block".into()))
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
        Some(&Value::String("alpha_test".into()))
    );
    assert_eq!(
        compound(materials, "up").get("texture"),
        Some(&Value::String("test_lamp_top".into()))
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
    // Every definition round-trips through the codec.
    let packet = Packet::StartGame(Box::new(crate::protocol::packet::StartGame {
        blocks,
        ..Default::default()
    }));
    let mut bytes = Vec::new();
    packet.encode(&mut bytes, 0).unwrap();
    let mut budget = usize::MAX;
    assert_eq!(
        Packet::decode(packet.id(), &bytes, &mut budget, 0).unwrap(),
        packet
    );
}

/// The default creative inventory with nothing registered: PMMP's without the Education items.
fn vanilla_content() -> crate::protocol::packet::CreativeContent {
    creative::Resolved::vanilla().content().clone()
}

/// The group index PMMP's rule gives the next entry of `category` without a group after
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

// pmmp: CreativeCategory, CreativeInventoryCache::buildPacket
#[test]
fn the_creative_inventory_lists_custom_entries_after_pmmps() {
    let r = registry();
    let vanilla = vanilla_content();
    let content = r.creative_content();
    let n = vanilla.items.len();
    // The ruby and the apple share their group (same name and icon, PMMP: the same group
    // object) though the ore comes between them; the ore joins the anonymous nature group
    // PMMP's rule leaves open.
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
    assert_eq!(named.icon, icon, "the first item, with PMMP's group index");
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
fn finished(builder: crate::ServerBuilder) -> Result<Registry, Error> {
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

// pmmp: CreativeInventory, CreativeInventory::add, CreativeInventory::remove, CreativeInventory::clear, CreativeInventoryEntry::matchesItem, CreativeGroup, CreativeGroup::__construct, CreativeGroup::getName, CreativeGroup::getIcon
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
                    // No entry has this damage (PMMP compares it, not the NBT).
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
    let pmmp = vanilla_content();
    // The default: PMMP's without its first oak planks, then the ruby's place.
    let default = r.creative_content();
    let planks_id = i32::from(planks.runtime_id());
    let mut want = listed(&pmmp);
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

fn finish(build: impl FnOnce(&mut Registry)) -> Result<(), Error> {
    let mut r = Registry::default();
    build(&mut r);
    r.finish()
}

static MINECRAFT: Item = Item::new("minecraft:ruby", "Ruby", "ruby");
static NO_NAMESPACE: Item = Item::new("ruby", "Ruby", "ruby");
static UPPER: Item = Item::new("test:Ruby", "Ruby", "ruby");
static BIG_STACK: Item = Item::new("test:big", "Big", "big").with_max_stack_size(65);
static WORN_STACK: Item = Item::new("test:worn", "Worn", "worn")
    .with_durability(5)
    .with_max_stack_size(16);
static UNSORTED: Block = Block::new("test:unsorted", "x")
    .with_properties(&[Property::bool("lit"), Property::int("level", 0, 1)]);
static WIDE: Block = Block::new("test:wide", "x").with_properties(&[Property::int("level", 0, 16)]);
static BRIGHT: Block = Block::new("test:bright", "x").with_light_emission(16);
static OUTSIDE: Block = Block::new("test:outside", "x").with_collision_box(Some(Aabb::new(
    Vec3::new(0.0, 0.0, 0.0),
    Vec3::new(1.0, 1.5, 1.0),
)));
static ORPHAN_DROP: Block =
    Block::new("test:orphan", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&RUBY), 1)]));
static SWORDS_DROP: Block =
    Block::new("test:swords", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&SWORD), 2)]));
static SWORD_DROP: Block =
    Block::new("test:one_sword", "x").with_drops(Drops::Items(&[(ItemRef::Custom(&SWORD), 1)]));

/// Blocks whose one state hashes, as found by a search, to the network ID of a reference state
/// that is not a state of the table: an inert state's and an alias's.
static AS_INERT: Block = Block::new("test:c257583", "x");
static AS_ALIAS: Block = Block::new("test:c26250", "x");

#[test]
fn a_state_with_the_network_id_of_any_vanilla_state_refuses_the_build() {
    for block in [&AS_INERT, &AS_ALIAS] {
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
fn invalid_recipes_refuse_the_build() {
    let diamond = Ingredient::Item(vanilla(item::Kind::Diamond));
    let logs = Ingredient::Tag(item::Tag::find("minecraft:logs").unwrap());
    let out = [(vanilla(item::Kind::DiamondHoe), 1)];
    let bad = [
        Recipe::shaped(&[], &[], &out),
        Recipe::shaped(&["DDDD"], &[('D', diamond)], &out),
        Recipe::shaped(&["DD", "D"], &[('D', diamond)], &out),
        Recipe::shaped(&["DX"], &[('D', diamond)], &out),
        Recipe::shaped(&["D"], &[('D', diamond), ('D', diamond)], &out),
        Recipe::shaped(&["  "], &[], &out),
        Recipe::shaped(&["D"], &[('D', diamond)], &[]),
        Recipe::shaped(
            &["D"],
            &[('D', diamond)],
            &[(vanilla(item::Kind::DiamondHoe), 2)],
        ),
        Recipe::shapeless(&[], &out),
        Recipe::shapeless(&[diamond; 10], &out),
        Recipe::shapeless(&[Ingredient::Item(ItemRef::Custom(&RUBY))], &out),
        Recipe::furnace(
            FurnaceType::Furnace,
            diamond,
            (vanilla(item::Kind::Stick), 0),
        ),
        Recipe::stonecutter(diamond, (vanilla(item::Kind::Stick), 65)),
        Recipe::smithing_transform(diamond, diamond, logs, ItemRef::Custom(&SWORD)),
        Recipe::brewing(logs, diamond, vanilla(item::Kind::Potion)),
        Recipe::brewing(diamond, logs, vanilla(item::Kind::Potion)),
        Recipe::brewing(diamond, diamond, ItemRef::Custom(&RUBY)),
        Recipe::brewing_container(
            vanilla(item::Kind::Potion),
            logs,
            vanilla(item::Kind::Potion),
        ),
        Recipe::brewing_container(ItemRef::Custom(&RUBY), diamond, vanilla(item::Kind::Potion)),
    ];
    for (i, recipe) in bad.into_iter().enumerate() {
        let e = finish(|r| r.add_recipe(recipe)).unwrap_err();
        assert!(matches!(e, Error::Recipe { index: 0, .. }), "{i}: {e}");
    }
}

#[test]
fn recipe_refusals_name_the_limits_they_enforce() {
    use super::recipe::{MAX_INGREDIENTS, MAX_RESULTS, MAX_SIDE};
    let diamond = Ingredient::Item(vanilla(item::Kind::Diamond));
    let out = [(vanilla(item::Kind::DiamondHoe), 1)];
    let too_many = [(vanilla(item::Kind::Stick), 1); MAX_RESULTS + 1];
    let wide = "D".repeat(MAX_SIDE + 1);
    let results = format!("no result or more than {MAX_RESULTS}");
    let cases = [
        (
            Recipe::shapeless(&[diamond; MAX_INGREDIENTS + 1], &out),
            format!("no ingredient or more than {MAX_INGREDIENTS}"),
        ),
        (
            Recipe::shaped(&[&wide], &[('D', diamond)], &out),
            format!("the shape is empty or larger than {MAX_SIDE} by {MAX_SIDE}"),
        ),
        (Recipe::shapeless(&[diamond], &too_many), results.clone()),
        (
            Recipe::shaped(&["D"], &[('D', diamond)], &too_many),
            results,
        ),
    ];
    for (recipe, expected) in cases {
        let e = finish(|r| r.add_recipe(recipe)).unwrap_err();
        let Error::Recipe { reason, .. } = e else {
            panic!("{e}");
        };
        assert_eq!(reason, expected);
    }
}

static AIR_DROP: Block = Block::new("test:air_drop", "x")
    .with_drops(Drops::Items(&[(ItemRef::Block(block::Kind::Air), 1)]));

#[test]
fn results_and_drops_naming_no_item_say_so() {
    let diamond = Ingredient::Item(vanilla(item::Kind::Diamond));
    for result in [
        (ItemRef::Custom(&RUBY), 1),
        (ItemRef::Block(block::Kind::Air), 1),
    ] {
        for recipe in [
            Recipe::shapeless(&[diamond], &[result]),
            Recipe::shaped(&["D"], &[('D', diamond)], &[result]),
            Recipe::stonecutter(diamond, result),
            Recipe::furnace(FurnaceType::Furnace, diamond, result),
            Recipe::smithing_transform(diamond, diamond, diamond, result.0),
        ] {
            assert_eq!(
                finish(|r| r.add_recipe(recipe)),
                Err(Error::Recipe {
                    index: 0,
                    reason: "a result names an unknown or unregistered item",
                })
            );
        }
    }
    assert_eq!(
        finish(|r| r.add_recipe(Recipe::furnace(
            FurnaceType::Furnace,
            diamond,
            (vanilla(item::Kind::Stick), 0),
        ))),
        Err(Error::Recipe {
            index: 0,
            reason: "a result count outside its stack size",
        })
    );
    let e = finish(|r| r.add_block(&AIR_DROP)).unwrap_err();
    assert_eq!(
        e.to_string(),
        "`test:air_drop` drops an unknown or unregistered item"
    );
}

fn grid(cells: &[Option<item::Type>]) -> Vec<Stack> {
    cells
        .iter()
        .map(|c| c.map_or(Stack::EMPTY, |t| Stack::new(t, 1)))
        .collect()
}

/// A shaped sword, a shapeless diamond and a blast-furnace ruby, registered on a builder.
fn recipes() -> Registry {
    let ruby = ItemRef::Custom(&RUBY);
    let stick = item::Kind::Stick.default_type();
    let builder = crate::Server::builder()
        .custom_item(&RUBY)
        .custom_item(&SWORD)
        .recipe(Recipe::shaped(
            &["R ", "RS"],
            &[
                ('R', Ingredient::Item(ruby)),
                ('S', Ingredient::Item(ItemRef::Item(stick))),
            ],
            &[(ItemRef::Custom(&SWORD), 1)],
        ))
        .recipe(Recipe::shapeless(
            &[
                Ingredient::Item(ruby),
                Ingredient::AnyMeta(vanilla(item::Kind::Coal)),
            ],
            &[(vanilla(item::Kind::Diamond), 2)],
        ))
        .recipe(Recipe::furnace(
            FurnaceType::BlastFurnace,
            Ingredient::Item(ItemRef::CustomBlock(&ORE)),
            (ruby, 3),
        ))
        .custom_block(&ORE);
    let mut r = builder.setup.custom;
    r.finish().unwrap();
    r
}

// pmmp: CraftingManager::registerShapedRecipe, CraftingManager::registerShapelessRecipe, FurnaceRecipeManager::register, ShapedRecipe::__construct, ShapelessRecipe::__construct, FurnaceRecipe::__construct, ExactRecipeIngredient::__construct
#[test]
fn custom_recipes_are_numbered_after_pmmps_and_judged_by_shape() {
    let r = recipes();
    let stick = item::Kind::Stick.default_type();
    let first = crate::crafting::first_special_id();
    assert_eq!(r.first_special_id(), first + 3);
    let ruby_t = r.item_type(&RUBY).unwrap();
    let Some(Judged::Shaped(shaped)) = r.recipe(first) else {
        panic!("the first custom recipe is shaped");
    };
    assert_eq!(shaped.size(), (2, 2));
    // As placed, mirrored, moved in the grid; not upside down.
    let at = |cells: [Option<item::Type>; 9]| shaped.matches(&grid(&cells), 3);
    let (r_, s_) = (Some(ruby_t), Some(stick));
    assert!(at([r_, None, None, r_, s_, None, None, None, None]));
    assert!(
        at([None, r_, None, s_, r_, None, None, None, None]),
        "mirrored"
    );
    assert!(
        at([None, None, None, None, r_, None, None, r_, s_]),
        "moved"
    );
    assert!(
        !at([r_, s_, None, r_, None, None, None, None, None]),
        "upside down"
    );
    assert!(!at([r_, None, None, r_, r_, None, None, None, None]));
    assert_eq!(
        shaped.results[0].to_item(),
        Some(Stack::new(r.item_type(&SWORD).unwrap(), 1))
    );
    let Some(Judged::Shapeless(shapeless)) = r.recipe(first + 1) else {
        panic!("the second custom recipe is shapeless");
    };
    let charcoal = item::Type::from_name("minecraft:charcoal", 0, None, &Compound::new());
    let coal = item::Kind::Coal.default_type();
    assert!(shapeless.matches(&grid(&[Some(coal), None, r_])));
    assert!(!shapeless.matches(&grid(&[Some(coal), Some(coal)])));
    assert!(charcoal.is_some());
    assert!(!shapeless.matches(&grid(&[charcoal, r_])), "another name");
    assert!(matches!(r.recipe(first + 2), Some(Judged::Furnace)));
    assert!(r.recipe(first + 3).is_none());
    assert!(
        matches!(r.recipe(1), Some(Judged::Shaped(_))),
        "PMMP's come first"
    );
}

/// The item type `name` with `meta`.
fn typed(name: &str, meta: u16) -> item::Type {
    item::Type::from_name(name, meta, None, &Compound::new()).unwrap()
}

/// A server with a stonecutter, a potion type, a smithing and two container change recipes of
/// its own, registered in that order.
fn station_and_brewing() -> Registry {
    let ruby = ItemRef::Custom(&RUBY);
    let wart = ItemRef::Item(typed("minecraft:nether_wart", 0));
    let lingering = ItemRef::Item(typed("minecraft:lingering_potion", 0));
    let potion = vanilla(item::Kind::Potion);
    finished(
        crate::Server::builder()
            .custom_item(&RUBY)
            .custom_item(&SWORD)
            .recipe(Recipe::stonecutter(
                Ingredient::Item(ruby),
                (vanilla(item::Kind::Diamond), 2),
            ))
            .recipe(Recipe::brewing(
                Ingredient::Item(ItemRef::Item(typed("minecraft:splash_potion", 0))),
                Ingredient::Item(ruby),
                ItemRef::Item(typed("minecraft:splash_potion", 7)),
            ))
            .recipe(Recipe::smithing_transform(
                Ingredient::Item(ItemRef::Item(typed(
                    "minecraft:netherite_upgrade_smithing_template",
                    0,
                ))),
                Ingredient::Item(vanilla(item::Kind::DiamondSword)),
                Ingredient::AnyMeta(ruby),
                ItemRef::Custom(&SWORD),
            ))
            .recipe(Recipe::brewing_container(
                potion,
                Ingredient::Item(ruby),
                lingering,
            ))
            // Before PMMP's water + nether wart (a potion type change).
            .recipe(Recipe::brewing_container(
                potion,
                Ingredient::Item(wart),
                lingering,
            )),
    )
    .unwrap()
}

// pmmp: ShapelessRecipe::getType, ShapelessRecipeType
#[test]
fn stonecutter_and_smithing_recipes_are_numbered_and_sent_after_pmmps() {
    let ruby = ItemRef::Custom(&RUBY);
    let cut = Recipe::stonecutter(Ingredient::Item(ruby), (ruby, 1));
    assert_eq!(cut.shapeless_type(), Some(ShapelessType::Stonecutter));
    assert_eq!(
        Recipe::shapeless(&[Ingredient::Item(ruby)], &[(ruby, 1)]).shapeless_type(),
        Some(ShapelessType::Crafting)
    );
    let smith = Ingredient::Item(ruby);
    assert_eq!(
        Recipe::smithing_transform(smith, smith, smith, ruby).shapeless_type(),
        None
    );
    let r = station_and_brewing();
    let first = crate::crafting::first_special_id();
    // The stonecutter and smithing recipes have network IDs, the brewing ones none.
    assert_eq!(r.first_special_id(), first + 2);
    let Some(Judged::Shapeless(s)) = r.recipe(first) else {
        panic!("a stonecutter recipe is shapeless");
    };
    assert_eq!(s.kind, ShapelessType::Stonecutter);
    let Some(Judged::SmithingTransform(t)) = r.recipe(first + 1) else {
        panic!("the smithing recipe");
    };
    assert_eq!(t.result.item, r.item_type(&SWORD).unwrap());
    let data = r.crafting_data();
    let pmmp = crate::crafting::crafting_data();
    let cut = data.shapeless_recipes.last().unwrap();
    assert_eq!(
        (cut.recipe_network_id, cut.block.as_str()),
        (first, "stonecutter")
    );
    let smith = data.smithing_transform_recipes.last().unwrap();
    assert_eq!(smith.recipe_network_id, first + 1);
    assert_eq!(
        data.smithing_transform_recipes.len(),
        pmmp.smithing_transform_recipes.len() + 1
    );
}

// pmmp: CraftingManager::registerPotionTypeRecipe, CraftingManager::registerPotionContainerChangeRecipe, PotionTypeRecipe::__construct, PotionContainerChangeRecipe::__construct, CraftingManager::matchBrewingRecipe
#[test]
fn brewing_recipes_are_sent_and_brew_after_pmmps_of_their_kind() {
    let r = station_and_brewing();
    let data = r.crafting_data();
    let pmmp = crate::crafting::crafting_data();
    let ruby_id = i32::from(r.item_type(&RUBY).unwrap().runtime_id());
    let splash_id = i32::from(typed("minecraft:splash_potion", 0).runtime_id());
    assert_eq!(data.potion_recipes.len(), pmmp.potion_recipes.len() + 1);
    assert_eq!(
        data.potion_recipes.last(),
        Some(&crate::protocol::PotionRecipe {
            input_potion_id: splash_id,
            input_potion_metadata: 0,
            reagent_item_id: ruby_id,
            reagent_item_metadata: 0,
            output_potion_id: splash_id,
            output_potion_metadata: 7,
        })
    );
    let potion_id = i32::from(item::Kind::Potion.default_type().runtime_id());
    let lingering_id = i32::from(typed("minecraft:lingering_potion", 0).runtime_id());
    assert_eq!(
        data.potion_container_change_recipes[pmmp.potion_container_change_recipes.len()..],
        [
            crate::protocol::PotionContainerChangeRecipe {
                input_item_id: potion_id,
                reagent_item_id: ruby_id,
                output_item_id: lingering_id,
            },
            crate::protocol::PotionContainerChangeRecipe {
                input_item_id: potion_id,
                reagent_item_id: i32::from(typed("minecraft:nether_wart", 0).runtime_id()),
                output_item_id: lingering_id,
            },
        ]
    );
    // Brewing: PMMP's container changes, the server's, PMMP's potion types, the server's.
    let one = |name: &str, meta: u16| Stack::new(typed(name, meta), 1);
    let ruby_one = Stack::new(r.item_type(&RUBY).unwrap(), 1);
    assert_eq!(
        r.brew(&one("minecraft:splash_potion", 0), &ruby_one),
        Some(one("minecraft:splash_potion", 7))
    );
    assert_eq!(
        r.brew(&one("minecraft:potion", 4), &ruby_one),
        Some(one("minecraft:lingering_potion", 4)),
        "the meta (potion type) stays"
    );
    assert_eq!(
        r.brew(
            &one("minecraft:potion", 0),
            &one("minecraft:nether_wart", 0)
        ),
        Some(one("minecraft:lingering_potion", 0)),
        "a container change comes before PMMP's potion types"
    );
    assert_eq!(
        r.brew(&one("minecraft:potion", 0), &one("minecraft:gunpowder", 0)),
        Some(one("minecraft:splash_potion", 0)),
        "PMMP's own still brew"
    );
    assert_eq!(r.brew(&one("minecraft:splash_potion", 7), &ruby_one), None);
    assert_eq!(r.brew(&ruby_one, &ruby_one), None);
}

#[test]
fn custom_furnace_lookups_by_name_and_tag_take_the_first_registered() {
    let ruby = ItemRef::Custom(&RUBY);
    let logs = item::Tag::find("minecraft:logs").unwrap();
    let oak = ItemRef::Block(block::Kind::OakLog);
    // A smoker has no PMMP recipe for logs or rubies: only the custom ones answer.
    let builder = crate::Server::builder()
        .custom_item(&RUBY)
        .recipe(Recipe::furnace(
            FurnaceType::Furnace,
            Ingredient::Tag(logs),
            (ruby, 1),
        ))
        .recipe(Recipe::furnace(
            FurnaceType::Smoker,
            Ingredient::Item(ruby),
            (ruby, 2),
        ))
        .recipe(Recipe::furnace(
            FurnaceType::Smoker,
            Ingredient::Tag(logs),
            (ruby, 3),
        ))
        .recipe(Recipe::furnace(
            FurnaceType::Smoker,
            Ingredient::Item(oak),
            (ruby, 4),
        ))
        .recipe(Recipe::furnace(
            FurnaceType::Smoker,
            Ingredient::AnyMeta(ruby),
            (ruby, 5),
        ));
    let mut r = builder.setup.custom;
    r.finish().unwrap();
    let count = crate::crafting::furnace_count();
    let index = |kind, t: item::Type| {
        r.furnace_recipe(kind, &Stack::new(t, 1))
            .map(|i| usize::from(i) - count)
    };
    let (ruby_t, oak_t) = (r.item_type(&RUBY).unwrap(), r.resolve(oak).unwrap());
    let birch = r.resolve(ItemRef::Block(block::Kind::BirchLog)).unwrap();
    // The tag recipe comes before the named one for oak; birch has the tag only.
    assert_eq!(index(FurnaceType::Smoker, oak_t), Some(2));
    assert_eq!(index(FurnaceType::Smoker, birch), Some(2));
    assert_eq!(index(FurnaceType::Smoker, ruby_t), Some(1));
    assert_eq!(index(FurnaceType::BlastFurnace, ruby_t), None);
    assert_eq!(r.furnace_recipe(FurnaceType::Smoker, &Stack::EMPTY), None);
    // The same answers as a scan of every custom recipe in order.
    for kind in [
        FurnaceType::Furnace,
        FurnaceType::Smoker,
        FurnaceType::BlastFurnace,
    ] {
        for t in [ruby_t, birch] {
            let input = Stack::new(t, 1);
            let scan = (0..5).find(|&i| {
                let recipe = r.furnace_at(u16::try_from(count + i).unwrap()).unwrap();
                recipe.kind == kind && recipe.input.accepts(&input)
            });
            if crate::crafting::furnace_recipe(kind, &input).is_none() {
                assert_eq!(index(kind, t), scan, "{kind:?} {t:?}");
            }
        }
    }
}

#[test]
fn custom_furnace_recipes_count_after_pmmps_and_are_sent() {
    let r = recipes();
    let first = crate::crafting::first_special_id();
    let ruby_t = r.item_type(&RUBY).unwrap();
    // The furnace recipe takes the ore in a blast furnace only.
    let ore = Stack::new(r.resolve(ItemRef::CustomBlock(&ORE)).unwrap(), 1);
    let index = r.furnace_recipe(FurnaceType::BlastFurnace, &ore).unwrap();
    assert_eq!(usize::from(index), crate::crafting::furnace_count());
    assert_eq!(r.furnace_recipe(FurnaceType::Furnace, &ore), None);
    let cooked = r.furnace_at(index).unwrap().result.to_item();
    assert_eq!(cooked, Some(Stack::new(ruby_t, 3)));
    let iron = item::Type::from_name("minecraft:raw_iron", 0, None, &Compound::new()).unwrap();
    let pmmp = r
        .furnace_recipe(FurnaceType::BlastFurnace, &Stack::new(iron, 1))
        .unwrap();
    assert!(usize::from(pmmp) < crate::crafting::furnace_count());
    // CraftingData: PMMP's, then the three with their IDs.
    let data = r.crafting_data();
    let vanilla = crate::crafting::crafting_data();
    assert_eq!(data.shaped_recipes.len(), vanilla.shaped_recipes.len() + 1);
    assert_eq!(
        data.shapeless_recipes.len(),
        vanilla.shapeless_recipes.len() + 2
    );
    assert_eq!(data.shaped_recipes.last().unwrap().recipe_network_id, first);
    let ids: Vec<u32> = data
        .shapeless_recipes
        .iter()
        .rev()
        .take(2)
        .map(|s| s.recipe_network_id)
        .collect();
    assert_eq!(ids, [first + 2, first + 1]);
    assert_eq!(
        data.shapeless_recipes.last().unwrap().block,
        "blast_furnace"
    );
}

/// `vanilla-recipes = false`: only the server's own recipes exist, numbered from 1, in the
/// lookups and in `CraftingData`.
#[test]
fn without_vanilla_recipes_only_the_servers_own_exist() {
    let builder = crate::Server::builder()
        .custom_item(&RUBY)
        .recipe(Recipe::stonecutter(
            Ingredient::Item(ItemRef::Custom(&RUBY)),
            (vanilla(item::Kind::Diamond), 2),
        ));
    let mut r = builder.setup.custom;
    r.drop_vanilla_recipes();
    r.finish().unwrap();
    assert_eq!(r.first_special_id(), 2);
    assert!(matches!(r.recipe(1), Some(Judged::Shapeless(_))));
    assert!(r.recipe(2).is_none());
    let data = r.crafting_data();
    let counts = (
        data.shaped_recipes.len(),
        data.shapeless_recipes.len(),
        data.smithing_transform_recipes.len(),
        data.smithing_trim_recipes.len(),
        data.potion_recipes.len(),
        data.potion_container_change_recipes.len(),
    );
    assert_eq!(counts, (0, 1, 0, 0, 0, 0));
    assert_eq!(data.shapeless_recipes[0].recipe_network_id, 1);
    assert!(data.clear_recipes);
    assert_eq!(
        r.crafting_candidates("minecraft:oak_log")
            .collect::<Vec<_>>(),
        [1]
    );
    let iron = Stack::new(typed("minecraft:raw_iron", 0), 1);
    assert_eq!(r.furnace_recipe(FurnaceType::Furnace, &iron), None);
    let water = Stack::new(typed("minecraft:potion", 0), 1);
    let wart = Stack::new(typed("minecraft:nether_wart", 0), 1);
    assert_eq!(r.brew(&water, &wart), None);
    let mut with = Registry::default();
    with.finish().unwrap();
    assert!(with.furnace_recipe(FurnaceType::Furnace, &iron).is_some());
    assert!(with.brew(&water, &wart).is_some());
}

/// `recipe-book = false`: every recipe goes out locked with nothing that unlocks it (the
/// context BDS gives recipes only the server unlocks), so the client's book stays empty.
#[test]
fn a_hidden_recipe_book_sends_every_recipe_locked() {
    let mut r = Registry::default();
    r.hide_recipe_book();
    r.finish().unwrap();
    assert!(!r.has_recipe_book());
    let data = r.crafting_data();
    let locked = Some(crate::protocol::RecipeUnlockRequirement {
        context: 0,
        ingredients: Vec::new(),
    });
    assert!(
        data.shaped_recipes
            .iter()
            .all(|s| s.unlock_requirement == locked)
    );
    assert!(
        data.shapeless_recipes
            .iter()
            .all(|s| s.unlock_requirement == locked)
    );
    let vanilla = crate::crafting::crafting_data();
    assert_eq!(data.shaped_recipes.len(), vanilla.shaped_recipes.len());
    assert_eq!(
        data.shapeless_recipes.len(),
        vanilla.shapeless_recipes.len()
    );
    let mut shown = Registry::default();
    shown.finish().unwrap();
    assert!(shown.has_recipe_book());
}

/// No Education item in the creative inventory; PMMP's recipes make and take none (they come
/// from BDS's chemistry pack), so `CraftingData` needs no such filter.
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
    let pmmp = item::creative_content();
    assert!(
        education(&pmmp) > 100,
        "elements, compounds, hardened glass"
    );
    assert_eq!(education(&r.creative_content()), 0);
    assert_eq!(
        r.creative_content().items.len(),
        pmmp.items.len() - education(&pmmp)
    );
    let data = r.crafting_data();
    let takes_or_makes = data.shaped_recipes.iter().any(|s| {
        s.output.iter().any(is_education)
            || s.input.iter().any(|i| {
                matches!(&i.descriptor,
                crate::protocol::ItemDescriptor::Default(d) if item::is_education(&d.name))
            })
    }) || data.shapeless_recipes.iter().any(|s| {
        s.output.iter().any(is_education)
            || s.input.iter().any(|i| {
                matches!(&i.descriptor,
                crate::protocol::ItemDescriptor::Default(d) if item::is_education(&d.name))
            })
    });
    assert!(!takes_or_makes);
}

#[test]
fn nothing_registered_sends_exactly_the_vanilla_content() {
    let mut r = Registry::default();
    r.finish().unwrap();
    assert_eq!(r.item_registry(), item::registry());
    assert_eq!(r.creative_content(), vanilla_content());
    assert_eq!(r.crafting_data(), crate::crafting::crafting_data());
    assert!(r.block_entries().is_empty());
    assert_eq!(r.first_special_id(), crate::crafting::first_special_id());
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
    use crate::world::{Chunk, Entry};
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
