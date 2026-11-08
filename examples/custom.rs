//! A custom-content server, built on Anchor's public API only.
//!
//! - A custom item, the ruby, and `/ruby [count]`, which gives some.
//! - A custom block, the ruby block: a `glowing` property that using the block toggles, a
//!   hardness, and four rubies as its drops.
//! - A custom entity, the wisp, whose behaviour hops and counts the pokes it gets (in its saved
//!   custom data) when a player interacts with it.
//! - A villager shop with typed offers: a ruby for an emerald, two emeralds for a ruby.
//! - A task once a second puts the wisp and the shop next to the spawn when they are not there.
//! - A resource pack built in code: a manifest, the texture atlases and placeholder textures,
//!   zipped in memory and offered to every joining player.
//!
//! What it uses: [`ServerBuilder::custom_item`], [`ServerBuilder::custom_block`],
//! [`ServerBuilder::custom_entity`] with an [`entity::Behaviour`],
//! [`ServerBuilder::resource_pack`] with [`Pack::from_bytes`], [`Context::spawn_custom_entity`],
//! [`Context::spawn_villager`] and [`Context::set_trade_offers`].
//!
//! Run it in a folder of its own with `cargo run --example custom`: it reads `anchor.toml`
//! there, writing the default file first, and keeps the world in `data/`.
//!
//! [`ServerBuilder::custom_item`]: anchor::ServerBuilder::custom_item
//! [`ServerBuilder::custom_block`]: anchor::ServerBuilder::custom_block
//! [`ServerBuilder::custom_entity`]: anchor::ServerBuilder::custom_entity
//! [`ServerBuilder::resource_pack`]: anchor::ServerBuilder::resource_pack

use std::error::Error;
use std::num::NonZeroU32;

use anchor::block::{self, PropertyValue};
use anchor::command::{self, Command, Output, Param, Sender};
use anchor::custom::{self, Block, Category, Creative, Drops, Item, ItemRef, Offer, Property};
use anchor::entity::{self, Custom};
use anchor::event::{PlayerJoin, Priority};
use anchor::inventory::Kind;
use anchor::item::{self, Stack};
use anchor::math::Vec3;
use anchor::nbt::{Compound, Value};
use anchor::pack::Pack;
use anchor::player::{self, Message};
use anchor::world::{self, BlockPos};
use anchor::{Config, Context, Server};
use tracing::debug;

/// The villager's look (the actor variant): the cleric's robes.
const CLERIC: u8 = 7;
/// The language file's lines naming the ruby block and the wisp.
const EN_US_LANG: &str = "tile.example:ruby_block.name=Ruby Block\nentity.example:wisp.name=Wisp\n";
/// The ruby block's property.
const GLOWING: &str = "example:glowing";
/// The atlas of the item textures: the ruby's icon.
const ITEM_TEXTURES: &str = r#"{"resource_pack_name": "example", "texture_name": "atlas.items",
 "texture_data": {"example_ruby": {"textures": "textures/items/ruby"}}}"#;
/// The most rubies `/ruby` gives at once: one stack.
const MAX_RUBIES: u8 = 64;
/// The pack's manifest.
const PACK_MANIFEST: &str = r#"{
  "format_version": 2,
  "header": {
    "name": "Anchor custom example",
    "description": "Rubies and wisps",
    "uuid": "7c3b8e8a-55f5-4d8e-9a1e-2f6f0d3b6a01",
    "version": [1, 0, 0],
    "min_engine_version": [1, 21, 0]
  },
  "modules": [
    { "type": "resources", "uuid": "0d6c4e5b-2a7f-4b9c-8e3d-5f1a2b3c4d02", "version": [1, 0, 0] }
  ]
}"#;
/// The rubies `/ruby` gives without a count.
const RUBIES: i32 = 8;
/// The ruby. Its icon `example_ruby` is in the pack's `item_texture.json`.
static RUBY: Item =
    Item::new("example:ruby", "Ruby", "example_ruby").with_creative(Creative::new(Category::Items));
/// The ruby block. Its texture `example_ruby_block` is in the pack's `terrain_texture.json`.
static RUBY_BLOCK: Block = Block::new("example:ruby_block", "example_ruby_block")
    .with_properties(&[Property::bool(GLOWING)])
    .with_hardness(0.5)
    .with_drops(Drops::Items(&[(ItemRef::Custom(&RUBY), 4)]))
    .with_creative(Creative::new(Category::Construction))
    .with_behaviour(&Glow);
/// One second, in ticks.
const SECOND: NonZeroU32 = NonZeroU32::MIN.saturating_add(Context::TICKS_PER_SECOND - 1);
/// Where the shop's villager stands: next to the wisp.
const SHOP_AT: Vec3 = Vec3::new(0.5, -60.0, 2.5);
/// The atlas of the block textures: the ruby block's texture.
const TERRAIN_TEXTURES: &str = r#"{"resource_pack_name": "example", "texture_name": "atlas.terrain",
 "texture_data": {"example_ruby_block": {"textures": "textures/blocks/ruby_block"}}}"#;
/// The side of a placeholder texture, in pixels.
const TEXTURE_SIDE: u8 = 16;
/// The wisp's identifier.
const WISP: &str = "example:wisp";
/// Where the wisp stands: next to the spawn of the flat default world.
const WISP_AT: Vec3 = Vec3::new(-1.5, -60.0, 0.5);
/// The fields of a zip header after its signature: version 1.0, no flags, stored, 1980-01-01
/// 00:00.
const ZIP_HEADER: [u16; 5] = [10, 0, 0, 0, 0x21];
/// Why a zip cannot be written: a size or a count does not fit its field.
const ZIP_TOO_LARGE: &str = "the pack is too large for a plain zip";

/// What the ruby block does when used: it toggles its `glowing` property.
#[derive(Debug)]
struct Glow;

impl custom::Behaviour for Glow {
    fn on_use(
        &self,
        cx: &mut Context<'_>,
        player: player::Id,
        world: world::Id,
        pos: BlockPos,
        state: block::State,
    ) -> bool {
        let glowing = state.value(GLOWING) == Some(PropertyValue::Byte(1));
        let Some(next) = state.with(GLOWING, PropertyValue::Byte(u8::from(!glowing))) else {
            return false;
        };
        if let Some(world) = cx.world_mut(world) {
            or_log(world.set_block(pos, next));
        }
        let text = if glowing { "fades" } else { "glows" };
        tell(cx, player, format!("The ruby block {text}"));
        true
    }
}

/// What the wisp does: it hops when poked, and counts the pokes in its saved data.
#[derive(Debug)]
struct Wisp;

impl entity::Behaviour for Wisp {
    fn on_interact(&mut self, cx: &mut Context<'_>, wisp: entity::Id, player: player::Id) {
        let Some(mut entity) = cx.entity_mut(wisp) else {
            return;
        };
        let pokes = match entity.custom_data().and_then(|data| data.get("pokes")) {
            Some(Value::Int(n)) => n.saturating_add(1),
            _ => 1,
        };
        or_log(entity.set_custom_data(Compound::from([("pokes".to_owned(), Value::Int(pokes))])));
        or_log(entity.set_motion(Vec3::new(0.0, 0.4, 0.0)));
        tell(
            cx,
            player,
            format!("The wisp whispers: poked {pokes} times"),
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Server::builder()
        .config(Config::default().load("anchor.toml")?)
        .custom_item(&RUBY)
        .custom_block(&RUBY_BLOCK)
        .custom_entity(
            Custom::new(WISP)
                .with_size(0.5, 0.5)
                .with_name_tag("Wisp")
                .with_behaviour(Wisp),
        )
        .resource_pack(Pack::from_bytes(resource_pack()?)?)
        .command(ruby_command())
        .every(SECOND, place_wisp_and_shop)
        .on(Priority::Normal, on_join)
        .build()?
        .run()?;
    Ok(())
}

/// `/ruby [count]`: gives the player rubies.
fn ruby_command() -> Command {
    Command::new("ruby", "Gives you rubies").overload(
        &[Param::int("count").optional()],
        |cx, sender, args| {
            let &Sender::Player(id) = sender else {
                return Err(command::Error::failed("Only players can hold rubies"));
            };
            let count = u8::try_from(args.int("count").unwrap_or(RUBIES))
                .ok()
                .filter(|n| (1..=MAX_RUBIES).contains(n))
                .ok_or_else(|| {
                    command::Error::failed(format!("Give a count from 1 to {MAX_RUBIES}"))
                })?;
            let ruby = cx
                .custom_item(&RUBY)
                .ok_or_else(|| command::Error::failed("This server has no rubies"))?;
            let gift = Stack::new(ruby, count);
            let left = cx
                .player_mut(id)
                .map_or(gift.clone(), |p| p.inventory_mut(Kind::Main).add(&gift));
            Ok(Output::from(format!(
                "Gave you {} rubies",
                count.saturating_sub(left.count())
            )))
        },
    )
}

/// Puts the wisp and the shop next to the spawn unless they are there: both are saved with
/// their chunk, so after a restart they come back from the server file.
fn place_wisp_and_shop(cx: &mut Context<'_>) {
    let world = cx.default_world().id();
    if !cx
        .entities(world)
        .any(|(_, e)| e.custom_type() == Some(WISP))
    {
        or_log(cx.spawn_custom_entity(world, WISP_AT, WISP));
    }
    if !cx
        .entities(world)
        .any(|(id, _)| cx.trade_offers(id).is_some())
    {
        open_shop(cx, world);
    }
}

/// Spawns the villager and gives it its offers.
fn open_shop(cx: &mut Context<'_>, world: world::Id) {
    let Some(ruby) = cx.custom_item(&RUBY) else {
        return;
    };
    let emerald = item::Kind::Emerald.default_type();
    let offers = [
        Offer::new(Stack::new(ruby, 1), Stack::new(emerald, 1), 16),
        Offer::new(Stack::new(emerald, 2), Stack::new(ruby, 1), 16),
    ];
    match cx.spawn_villager(world, SHOP_AT, CLERIC, &[]) {
        Ok(villager) => or_log(cx.set_trade_offers(villager, &offers)),
        Err(error) => debug!(error = &error as &(dyn Error + 'static), "skipped"),
    }
}

fn on_join(event: &mut PlayerJoin, cx: &mut Context<'_>) {
    tell(
        cx,
        event.player(),
        "Welcome! /ruby gives rubies; the cleric trades them".to_owned(),
    );
}

/// Sends `text` to the player's chat.
fn tell(cx: &mut Context<'_>, player: player::Id, text: String) {
    if let Some(player) = cx.player_mut(player) {
        or_log(player.send(Message::Raw(text)));
    }
}

/// Logs a call that could not be done (a player left, a chunk is not loaded); the server goes
/// on.
fn or_log<T, E: Error + 'static>(result: Result<T, E>) {
    if let Err(error) = result {
        debug!(error = &error as &(dyn Error + 'static), "skipped");
    }
}

/// The resource pack: a manifest, the atlases naming the textures of the ruby and the ruby
/// block, the block's name and placeholder textures, as a zip archive. A real pack also holds
/// the wisp's client entity, model and texture.
fn resource_pack() -> Result<Vec<u8>, &'static str> {
    let ruby = png([224, 17, 95, 255])?;
    zip(&[
        ("manifest.json", PACK_MANIFEST.as_bytes()),
        ("textures/item_texture.json", ITEM_TEXTURES.as_bytes()),
        ("textures/terrain_texture.json", TERRAIN_TEXTURES.as_bytes()),
        ("textures/items/ruby.png", &ruby),
        ("textures/blocks/ruby_block.png", &ruby),
        ("texts/languages.json", br#"["en_US"]"#),
        ("texts/en_US.lang", EN_US_LANG.as_bytes()),
    ])
}

/// A zip archive of `files`, stored without compression.
fn zip(files: &[(&str, &[u8])]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for &(name, data) in files {
        let offset = u32::try_from(out.len()).map_err(|_| ZIP_TOO_LARGE)?;
        let size = u32::try_from(data.len()).map_err(|_| ZIP_TOO_LARGE)?;
        let name_len = u16::try_from(name.len()).map_err(|_| ZIP_TOO_LARGE)?;
        let sizes = [crc32(data), size, size];
        // The local header: the sizes, the name, no extra field; then the data.
        put32(&mut out, &[0x0403_4b50]);
        put16(&mut out, &ZIP_HEADER);
        put32(&mut out, &sizes);
        put16(&mut out, &[name_len, 0]);
        out.extend(name.as_bytes());
        out.extend(data);
        // The directory entry: no extra field, comment, disk or attributes; the local header.
        put32(&mut directory, &[0x0201_4b50]);
        put16(&mut directory, &[10]);
        put16(&mut directory, &ZIP_HEADER);
        put32(&mut directory, &sizes);
        put16(&mut directory, &[name_len, 0, 0, 0, 0]);
        put32(&mut directory, &[0, offset]);
        directory.extend(name.as_bytes());
    }
    let count = u16::try_from(files.len()).map_err(|_| ZIP_TOO_LARGE)?;
    let start = u32::try_from(out.len()).map_err(|_| ZIP_TOO_LARGE)?;
    let length = u32::try_from(directory.len()).map_err(|_| ZIP_TOO_LARGE)?;
    out.extend(directory);
    // The end record: one disk, the entries, the directory, no comment.
    put32(&mut out, &[0x0605_4b50]);
    put16(&mut out, &[0, 0, count, count]);
    put32(&mut out, &[length, start]);
    put16(&mut out, &[0]);
    Ok(out)
}

/// Appends `values` as little-endian 16-bit numbers.
fn put16(out: &mut Vec<u8>, values: &[u16]) {
    for v in values {
        out.extend(v.to_le_bytes());
    }
}

/// Appends `values` as little-endian 32-bit numbers.
fn put32(out: &mut Vec<u8>, values: &[u32]) {
    for v in values {
        out.extend(v.to_le_bytes());
    }
}

/// A square placeholder texture of one `colour` (RGBA): a PNG whose pixels are a stored zlib
/// stream.
fn png(colour: [u8; 4]) -> Result<Vec<u8>, &'static str> {
    let side = usize::from(TEXTURE_SIDE);
    let row: Vec<u8> = std::iter::once(0) // no filter
        .chain(colour.iter().copied().cycle().take(colour.len() * side))
        .collect();
    let pixels = row.repeat(side);
    let len = u16::try_from(pixels.len()).map_err(|_| "the texture is too large")?;
    // zlib header, one final stored block, the pixels and their Adler-32.
    let mut idat = vec![0x78, 0x01, 0x01];
    idat.extend(len.to_le_bytes());
    idat.extend((!len).to_le_bytes());
    idat.extend(&pixels);
    idat.extend(adler32(&pixels).to_be_bytes());
    let mut header = Vec::with_capacity(13);
    let side_bytes = u32::from(TEXTURE_SIDE).to_be_bytes();
    header.extend(side_bytes);
    header.extend(side_bytes);
    // 8 bits per channel, RGBA, deflate, adaptive filters, not interlaced.
    header.extend([8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", header), (b"IDAT", idat), (b"IEND", Vec::new())] {
        let len = u32::try_from(data.len()).map_err(|_| "the texture is too large")?;
        out.extend(len.to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(&data);
        let crc = crc32(out.get(start..).unwrap_or_default());
        out.extend(crc.to_be_bytes());
    }
    Ok(out)
}

/// CRC-32 (IEEE), as zip and PNG use it.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0_u32, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |c, _| {
            (c >> 1) ^ (0xedb8_8320 & (c & 1).wrapping_neg())
        })
    })
}

/// Adler-32, as zlib uses it.
fn adler32(bytes: &[u8]) -> u32 {
    let (a, b) = bytes.iter().fold((1_u32, 0_u32), |(a, b), &byte| {
        let a = (a + u32::from(byte)) % 65_521;
        (a, (b + a) % 65_521)
    });
    (b << 16) | a
}
