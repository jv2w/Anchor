//! Tests of custom content: registration and its checks, the representation of custom types and
//! states, the start-game data, recipes and stacks.

use super::*;
use crate::block::{self, PropertyValue, ToolType};
use crate::crafting::Recipe as Judged;
use crate::item::{self, Stack};
use crate::math::{Aabb, Vec3};
use crate::nbt::{Compound, List, Value};
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
    assert_eq!(properties.get("creative_category"), Some(&Value::Int(4)));
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

// pmmp: CreativeCategory
#[test]
fn the_creative_inventory_lists_custom_entries_after_pmmps() {
    let r = registry();
    let vanilla = item::creative_content();
    let content = r.creative_content();
    // PMMP groups runs: the ruby, the ore (an anonymous nature group), the apple (the ruby's
    // group name again, in a new run).
    assert_eq!(content.items.len(), vanilla.items.len() + 3);
    assert_eq!(content.groups.len(), vanilla.groups.len() + 3);
    let ruby = &content.items[vanilla.items.len()];
    assert_eq!(ruby.creative_item_network_id, item::creative_count() + 1);
    let ore = &content.items[vanilla.items.len() + 1];
    let apple = &content.items[vanilla.items.len() + 2];
    assert_ne!(
        ruby.group_index, ore.group_index,
        "another category starts a group"
    );
    assert_ne!(apple.group_index, ore.group_index);
    let anonymous = &content.groups[usize::try_from(ore.group_index).unwrap()];
    assert_eq!((anonymous.category, anonymous.name.as_str()), (2, ""));
    // The stacks the client takes come back as the custom items.
    let id = ruby.creative_item_network_id;
    assert_eq!(
        r.creative_stack(id),
        Some(Stack::new(r.item_type(&RUBY).unwrap(), 1))
    );
    assert_eq!(r.creative_stack(1), item::creative_stack(1));
    assert_eq!(r.creative_stack(id + 3), None);
}

#[test]
fn custom_stacks_round_trip_through_disk_and_network() {
    let r = registry();
    let sword = Stack::new(r.item_type(&SWORD).unwrap(), 1).with_damage(30);
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
    ];
    for (i, recipe) in bad.into_iter().enumerate() {
        let e = finish(|r| r.add_recipe(recipe)).unwrap_err();
        assert!(matches!(e, Error::Recipe { index: 0, .. }), "{i}: {e}");
    }
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

#[test]
fn nothing_registered_sends_exactly_the_vanilla_content() {
    let mut r = Registry::default();
    r.finish().unwrap();
    assert_eq!(r.item_registry(), item::registry());
    assert_eq!(r.creative_content(), item::creative_content());
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
/// `minecraft:info_update`) and is saved back exactly as it was (PLAN.md 7).
#[test]
fn saved_custom_blocks_load_back_or_stay_as_they_were_saved() {
    use crate::world::{BlockPos, Chunk, ChunkPos, Entry, Kind, World};
    let r = std::sync::Arc::new(registry());
    let blue = r
        .block_state(&LAMP)
        .and_then(|s| s.with("color", PropertyValue::String("blue")))
        .unwrap();
    let mut chunk = Chunk::new();
    chunk.put(1, -60, 2, Entry::Known(blue)).unwrap();
    let record = crate::save::to_record(&chunk);
    let saved = crate::save::to_chunk(record.clone()).unwrap();
    assert!(
        matches!(saved.entry(1, -60, 2), Some(Entry::Unknown(_))),
        "the save thread knows no custom block"
    );
    let pos = BlockPos::new(1, -60, 2);
    let mut world = World::new("w", Kind::Temporary, 0, BlockPos::new(0, 0, 0));
    world.set_custom(std::sync::Arc::clone(&r));
    assert!(world.custom().is_some());
    world.insert_chunk(ChunkPos::new(0, 0), saved);
    assert_eq!(world.block(pos), Some(blue));
    assert_eq!(
        world.chunk(ChunkPos::new(0, 0)).map(crate::save::to_record),
        Some(record.clone())
    );
    // Without the registration.
    let mut bare = World::new("w", Kind::Temporary, 0, BlockPos::new(0, 0, 0));
    bare.insert_chunk(
        ChunkPos::new(0, 0),
        crate::save::to_chunk(record.clone()).unwrap(),
    );
    assert_eq!(bare.block(pos), None);
    assert!(bare.custom().is_none());
    assert_eq!(
        bare.chunk(ChunkPos::new(0, 0)).map(crate::save::to_record),
        Some(record)
    );
}
