<?php

/*
 * Data dump. Run by `cargo xtask dump` with the dump PHP binary, from the pinned dump source
 * checkout (`.cache/dump`) as the working directory:
 *
 *     php dump.php <output directory> <reference blocks.json>
 *
 * It loads the dump source's own registries and writes what they hold as JSON, so the data is
 * what the dump source runs with, not a reading of its source. Every list is sorted, so the same
 * checkout writes the same bytes. Files:
 * - blocks.json: every block state of the dump source with its Bedrock serialization, light
 *   values, its class, parent classes and traits, break info, physical flags, friction, type
 *   tags, the support type of each face, the block-local collision boxes, its item form
 *   (Block::asItem), and, for blocks whose state the dump source works out from their neighbours
 *   (fences, panes, walls, fence gates, stairs, tripwire and its hooks), what that needs: the
 *   kind, the facing and, for fences, panes and stairs, the support and collision boxes of every
 *   variant the neighbours choose, keyed by the client properties that name the variant.
 * - deserialized.json: how the dump source's block state deserializer reads each state of the
 *   reference list (the second argument, written by `xtask/go/reference`), in its order: the
 *   Bedrock form of the state it becomes (`read_as`), or null where it rejects the state (and
 *   loads an unknown block). A state read as itself is left out, so the file lists what differs:
 *   the dump source ignores some vanilla properties (a pumpkin's facing, a hanging sign's unused
 *   facing) and writes one value for them.
 * - items.json: every item type with its Bedrock serialization, names and data (class, stack
 *   size, durability, tool, armour, food, cooldown and so on).
 * - creative.json: the creative inventory in order, with its groups.
 * - tables.json: status effects, potion types, enchantments, and banner patterns.
 *
 * NBT values are written as {"type": value} objects: byte, short, int, long, float, double,
 * string, byte_array, int_array, long_array, list ({"type": element type, "values": [...]}) and
 * compound (keys sorted). NBT floats are written as their bit patterns so the JSON is exact. Other
 * numbers (hardness, friction, boxes) are PHP floats written in their shortest exact form.
 *
 * A consumable's additional effects are deterministic if the call draws nothing from PHP's random
 * number generator (checked by comparing the generator's next value with and without the call)
 * or, if it draws, gives the same result for 4,096 seeds; otherwise they are written as
 * "random".
 */

declare(strict_types=1);

require 'vendor/autoload.php';

use pocketmine\block\Block;
use pocketmine\block\BlockTypeIds;
use pocketmine\block\Fence;
use pocketmine\block\FenceGate;
use pocketmine\block\RuntimeBlockStateRegistry;
use pocketmine\block\Stair;
use pocketmine\block\Thin;
use pocketmine\block\Tripwire;
use pocketmine\block\TripwireHook;
use pocketmine\block\utils\BannerPatternType;
use pocketmine\block\utils\StairShape;
use pocketmine\block\utils\SupportType;
use pocketmine\block\VanillaBlocks;
use pocketmine\block\Wall;
use pocketmine\color\Color;
use pocketmine\data\bedrock\BannerPatternTypeIdMap;
use pocketmine\data\bedrock\block\BlockStateData;
use pocketmine\data\bedrock\block\BlockStateDeserializeException;
use pocketmine\data\bedrock\EffectIdMap;
use pocketmine\data\bedrock\EnchantmentIdMap;
use pocketmine\data\bedrock\item\SavedItemData;
use pocketmine\data\bedrock\PotionTypeIdMap;
use pocketmine\entity\Consumable;
use pocketmine\entity\effect\Effect;
use pocketmine\entity\effect\EffectInstance;
use pocketmine\entity\effect\InstantEffect;
use pocketmine\entity\effect\VanillaEffects;
use pocketmine\entity\FoodSource;
use pocketmine\inventory\CreativeCategory;
use pocketmine\inventory\CreativeInventory;
use pocketmine\item\Armor;
use pocketmine\item\ConsumableItem;
use pocketmine\item\Durable;
use pocketmine\item\enchantment\VanillaEnchantments;
use pocketmine\item\Item;
use pocketmine\item\Potion;
use pocketmine\item\PotionType;
use pocketmine\item\ProjectileItem;
use pocketmine\item\SplashPotion;
use pocketmine\item\StringToItemParser;
use pocketmine\item\VanillaItems;
use pocketmine\lang\Translatable;
use pocketmine\math\Facing;
use pocketmine\nbt\tag\ByteArrayTag;
use pocketmine\nbt\tag\ByteTag;
use pocketmine\nbt\tag\CompoundTag;
use pocketmine\nbt\tag\DoubleTag;
use pocketmine\nbt\tag\FloatTag;
use pocketmine\nbt\tag\IntArrayTag;
use pocketmine\nbt\tag\IntTag;
use pocketmine\nbt\tag\ListTag;
use pocketmine\nbt\tag\LongTag;
use pocketmine\nbt\tag\ShortTag;
use pocketmine\nbt\tag\StringTag;
use pocketmine\nbt\tag\Tag;
use pocketmine\nbt\NBT;
use pocketmine\world\format\io\GlobalBlockStateHandlers;
use pocketmine\world\format\io\GlobalItemDataHandlers;

function fail(string $message) : never{
	fwrite(STDERR, "dump: $message\n");
	exit(1);
}

/** @return array<string, mixed> */
function nbt(Tag $tag) : array{
	return match(true){
		$tag instanceof ByteTag => ["byte" => $tag->getValue()],
		$tag instanceof ShortTag => ["short" => $tag->getValue()],
		$tag instanceof IntTag => ["int" => $tag->getValue()],
		$tag instanceof LongTag => ["long" => $tag->getValue()],
		$tag instanceof FloatTag => ["float" => unpack("V", pack("g", $tag->getValue()))[1]],
		$tag instanceof DoubleTag => ["double" => unpack("P", pack("e", $tag->getValue()))[1]],
		$tag instanceof StringTag => ["string" => $tag->getValue()],
		$tag instanceof ByteArrayTag => ["byte_array" => array_values(unpack("C*", $tag->getValue()) ?: [])],
		$tag instanceof IntArrayTag => ["int_array" => $tag->getValue()],
		$tag instanceof ListTag => ["list" => [
			"type" => listType($tag->getTagType()),
			"values" => array_map(fn(Tag $t) => nbt($t), $tag->getValue()),
		]],
		$tag instanceof CompoundTag => ["compound" => compound($tag)],
		default => fail("unsupported NBT tag " . get_class($tag)),
	};
}

function listType(int $type) : string{
	return match($type){
		NBT::TAG_End => "end",
		NBT::TAG_Byte => "byte",
		NBT::TAG_Short => "short",
		NBT::TAG_Int => "int",
		NBT::TAG_Long => "long",
		NBT::TAG_Float => "float",
		NBT::TAG_Double => "double",
		NBT::TAG_ByteArray => "byte_array",
		NBT::TAG_String => "string",
		NBT::TAG_List => "list",
		NBT::TAG_Compound => "compound",
		NBT::TAG_IntArray => "int_array",
		default => fail("unsupported NBT list type $type"),
	};
}

/** @return array<string, mixed> */
function compound(CompoundTag $tag) : array{
	$out = [];
	foreach($tag->getValue() as $key => $value){
		$out[(string) $key] = nbt($value);
	}
	ksort($out, SORT_STRING);
	return $out;
}

/** @return array<string, mixed> */
function blockState(BlockStateData $data) : array{
	$states = [];
	foreach($data->getStates() as $key => $value){
		$states[$key] = nbt($value);
	}
	ksort($states, SORT_STRING);
	return ["name" => $data->getName(), "states" => (object) $states];
}

/** The Bedrock form of an item type: name, meta, block state and NBT (the last two when present). */
function itemType(Item $item) : array{
	$data = GlobalItemDataHandlers::getSerializer()->serializeType($item);
	return savedItem($data);
}

/** @return array<string, mixed> */
function savedItem(SavedItemData $data) : array{
	$out = ["name" => $data->getName(), "meta" => $data->getMeta()];
	if($data->getBlock() !== null){
		$out["block"] = blockState($data->getBlock());
	}
	$tag = $data->getTag();
	if($tag !== null && $tag->count() > 0){
		$out["nbt"] = compound($tag);
	}
	return $out;
}

/** An item stack: its type, count and NBT (the stack's NBT holds names, enchantments and so on). */
function itemStack(Item $item) : array{
	$data = GlobalItemDataHandlers::getSerializer()->serializeStack($item);
	$out = savedItem($data->getTypeData());
	$out["count"] = $data->getCount();
	return $out;
}

function jsonKey(array $value) : string{
	return json_encode($value, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
}

/** Sorts a list of arrays by their JSON form, which orders them by every field. */
function sortByJson(array $list) : array{
	usort($list, fn(array $a, array $b) => strcmp(jsonKey($a), jsonKey($b)));
	return $list;
}

function oneLine(mixed $value) : string{
	return json_encode($value, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE);
}

/** A list with one entry per line, so diffs of the committed files stay readable. */
function lines(array $list) : string{
	if(count($list) === 0){
		return "[]";
	}
	return "[\n" . implode(",\n", array_map(fn($v) => oneLine($v), $list)) . "\n]";
}

function write(string $dir, string $file, string $json) : void{
	if(file_put_contents("$dir/$file", $json . "\n") === false){
		fail("cannot write $dir/$file");
	}
}

/** The horizontal faces, in the order the dump writes them. */
const HORIZONTAL = [Facing::NORTH => "north", Facing::EAST => "east", Facing::SOUTH => "south", Facing::WEST => "west"];

function facingName(int $facing) : string{
	return HORIZONTAL[$facing] ?? fail("facing $facing is not horizontal");
}

/** Every face in the dump source's `Facing` order (down, up, north, south, west, east). */
const FACES = [Facing::DOWN, Facing::UP, Facing::NORTH, Facing::SOUTH, Facing::WEST, Facing::EAST];

/** The support type of each face (Block::getSupportType), in FACES order. @return string[] */
function support(Block $block) : array{
	return array_map(fn(int $facing) => strtolower($block->getSupportType($facing)->name), FACES);
}

/**
 * The block-local collision boxes (Block::recalculateCollisionBoxes, before the dump source moves them to
 * the block's position and model offset), each as [minX, minY, minZ, maxX, maxY, maxZ].
 * @return float[][]
 */
function boxes(Block $block) : array{
	$boxes = (fn() => $this->recalculateCollisionBoxes())->call($block);
	return array_map(fn($bb) => [$bb->minX, $bb->minY, $bb->minZ, $bb->maxX, $bb->maxY, $bb->maxZ], $boxes);
}

/**
 * What it takes to work out the block's state from its neighbours (Block::readStateFromWorld,
 * Wall::recalculateConnections, FenceGate::checkInWall), or null. A fence's group is its class: a
 * fence connects to the fences of its own class (`instanceof static`).
 */
function connect(Block $block) : ?array{
	return match(true){
		$block instanceof Fence => ["kind" => "fence", "group" => (new \ReflectionClass($block))->getShortName(), "variants" => connectionVariants($block)],
		$block instanceof Thin => ["kind" => "thin", "variants" => connectionVariants($block)],
		$block instanceof Wall => ["kind" => "wall"],
		$block instanceof FenceGate => ["kind" => "fence_gate", "facing" => facingName($block->getFacing())],
		$block instanceof Stair => [
			"kind" => "stair",
			"facing" => facingName($block->getFacing()),
			"upside_down" => $block->isUpsideDown(),
			"variants" => stairVariants($block),
		],
		$block instanceof Tripwire => ["kind" => "tripwire"],
		$block instanceof TripwireHook => ["kind" => "tripwire_hook", "facing" => facingName($block->getFacing())],
		default => null,
	};
}

/** The support and collision boxes of a block as one neighbour-chosen variant. */
function variant(array $properties, Block $block) : array{
	return ["properties" => $properties, "support" => support($block), "boxes" => boxes($block)];
}

/** A stair in each shape, by the client's `minecraft:corner` value. @return array[] */
function stairVariants(Stair $stair) : array{
	$variants = [];
	foreach([
		"none" => StairShape::STRAIGHT,
		"inner_left" => StairShape::INNER_LEFT,
		"inner_right" => StairShape::INNER_RIGHT,
		"outer_left" => StairShape::OUTER_LEFT,
		"outer_right" => StairShape::OUTER_RIGHT,
	] as $corner => $shape){
		$variants[] = variant(["minecraft:corner" => ["string" => $corner]], (clone $stair)->setShape($shape));
	}
	return $variants;
}

/**
 * A fence or pane with each set of horizontal connections (the `connections` it fills in
 * readStateFromWorld), by the client's `minecraft:connection_*` values. @return array[]
 */
function connectionVariants(Block $block) : array{
	$variants = [];
	for($set = 0; $set < 16; $set++){
		$connections = [];
		$properties = [];
		foreach(array_keys(HORIZONTAL) as $bit => $facing){
			$on = ($set >> $bit) & 1;
			if($on === 1){
				$connections[$facing] = true;
			}
			$properties["minecraft:connection_" . HORIZONTAL[$facing]] = ["byte" => $on];
		}
		ksort($properties, SORT_STRING);
		$copy = clone $block;
		(fn() => $this->connections = $connections)->call($copy);
		$variants[] = variant($properties, $copy);
	}
	return $variants;
}

/** The short names of a class and its parents, leaf first, stopping before `$base`. @return string[] */
function classChain(object $object, string $base) : array{
	$chain = [];
	for($class = new \ReflectionClass($object); $class !== false && $class->getName() !== $base; $class = $class->getParentClass()){
		// VanillaBlocks makes some types (stone) as anonymous classes; their names are file positions.
		if(!$class->isAnonymous()){
			$chain[] = $class->getShortName();
		}
	}
	return $chain;
}

/**
 * The traits and interfaces of a block or item class within `$namespace`, by short name with
 * any `Trait` suffix taken off, so a trait and the interface it implements are one name.
 * @return string[]
 */
function traits(object $object, string $namespace) : array{
	$names = [];
	$add = function(string $name) use (&$names, $namespace) : void{
		if(str_starts_with($name, $namespace)){
			$short = substr($name, strrpos($name, "\\") + 1);
			$names[str_ends_with($short, "Trait") ? substr($short, 0, -5) : $short] = true;
		}
	};
	foreach(class_implements($object) as $interface){
		$add($interface);
	}
	$pending = [get_class($object), ...array_values(class_parents($object))];
	while(count($pending) > 0){
		foreach(class_uses(array_pop($pending)) as $trait){
			$add($trait);
			$pending[] = $trait;
		}
	}
	$names = array_keys($names);
	sort($names, SORT_STRING);
	return $names;
}

/**
 * Runs `$f` after seeding PHP's random number generator with `$seed`. Returns its result and
 * whether it drew from the generator: the generator's next value differs from the seed's first.
 */
function seeded(callable $f, int $seed) : array{
	mt_srand($seed);
	$result = $f();
	$next = mt_rand();
	mt_srand($seed);
	return [$result, $next !== mt_rand()];
}

/**
 * The seeds a result that draws randomness is tried with.
 */
const SEEDS = 4096;

/**
 * The result of `$f` if it is the same for every seed: at once if it draws nothing from the
 * generator, else if it is the same for SEEDS seeds (a random count of 1 to 1); otherwise
 * "random".
 */
function deterministic(callable $f) : mixed{
	[$first, $random] = seeded($f, 0);
	if(!$random){
		return $first;
	}
	// Compared as JSON: block states hold objects, which `!==` compares by identity.
	$key = oneLine($first);
	for($seed = 1; $seed < SEEDS; $seed++){
		[$result] = seeded($f, $seed);
		if(oneLine($result) !== $key){
			return "random";
		}
	}
	return $first;
}

$out = $argv[1] ?? fail("usage: php dump.php <output directory> <reference blocks.json>");
$referencePath = $argv[2] ?? fail("usage: php dump.php <output directory> <reference blocks.json>");
if(!is_dir($out)){
	fail("$out is not a directory");
}

// Blocks: every state the runtime registry knows. Names are the VanillaBlocks registry names.
$typeNames = [];
$defaultStates = [];
foreach(VanillaBlocks::getAll() as $name => $block){
	$typeNames[$block->getTypeId()] = strtolower($name);
	$defaultStates[$block->getStateId()] = true;
}
$serializer = GlobalBlockStateHandlers::getSerializer();
$blocks = [];
foreach(RuntimeBlockStateRegistry::getInstance()->getAllKnownStates() as $stateId => $block){
	$info = $block->getBreakInfo();
	$item = $block->asItem();
	$entry = [
		"type" => $typeNames[$block->getTypeId()] ?? fail("block type {$block->getTypeId()} has no name"),
		"bedrock" => blockState($serializer->serialize($stateId)),
		// The state VanillaBlocks gives for the type.
		"default" => isset($defaultStates[$stateId]),
		"light_emission" => $block->getLightLevel(),
		"light_filter" => $block->getLightFilter(),
		"blocks_direct_sky_light" => $block->blocksDirectSkyLight(),
		"classes" => classChain($block, Block::class),
		"traits" => traits($block, "pocketmine\\block\\"),
		"break" => [
			"hardness" => $info->getHardness(),
			"blast_resistance" => $info->getBlastResistance(),
			"tool_type" => $info->getToolType(),
			"harvest_level" => $info->getToolHarvestLevel(),
		],
		// The flags that are true.
		"flags" => array_keys(array_filter([
			"solid" => $block->isSolid(),
			"transparent" => $block->isTransparent(),
			"replaceable" => $block->canBeReplaced(),
			"flowable" => $block->canBeFlowedInto(),
			"entity_collision" => $block->hasEntityCollision(),
		])),
		"friction" => $block->getFrictionFactor(),
		// The tag names without their namespace prefix.
		"tags" => array_map(fn(string $tag) => str_replace("pocketmine:", "", $tag), $block->getTypeTags()),
		"support" => support($block),
		"boxes" => boxes($block),
		"item" => $item->isNull() ? null : itemType($item),
	];
	sort($entry["tags"], SORT_STRING);
	$connect = connect($block);
	if($connect !== null){
		$entry["connect"] = $connect;
	}
	$blocks[] = $entry;
}
write($out, "blocks.json", lines(sortByJson($blocks)));

// The reference list as the dump source reads it when it loads a world
// (LevelDB::deserializeBlockPalette, after the upgrade, which the reference states no longer need).
$deserializer = GlobalBlockStateHandlers::getDeserializer();
$deserialized = [];
$reference = json_decode(file_get_contents($referencePath) ?: fail("cannot read $referencePath"), true, 512, JSON_THROW_ON_ERROR);
foreach($reference as $entry){
	$states = [];
	foreach($entry["states"] as $key => $typed){
		$value = reset($typed);
		$states[$key] = match(key($typed)){
			"byte" => new ByteTag($value),
			"int" => new IntTag($value),
			"string" => new StringTag($value),
			default => fail("reference state {$entry["name"]} has a property of type " . key($typed)),
		};
	}
	$input = BlockStateData::current($entry["name"], $states);
	try{
		$read = blockState($serializer->serialize($deserializer->deserialize($input)));
	}catch(BlockStateDeserializeException){
		$read = null;
	}
	$self = blockState($input);
	if(oneLine($read) !== oneLine($self)){
		$deserialized[] = ["name" => $entry["name"], "states" => $self["states"], "read_as" => $read];
	}
}
write($out, "deserialized.json", lines($deserialized));

// Items: the VanillaItems registry, the item form of every block state, and every name
// StringToItemParser knows, merged by Bedrock form. Items merged into one form must agree on
// their data.
$itemKinds = [];
foreach(VanillaItems::getAll() as $name => $item){
	$itemKinds[$item->getTypeId()] = strtolower($name);
}
$effectNames = [];
foreach(VanillaEffects::getAll() as $name => $effect){
	$effectNames[spl_object_id($effect)] = strtolower($name);
}

/** @param EffectInstance[] $list */
function effectList(array $list, array $names) : array{
	return array_map(fn(EffectInstance $e) => [
		"effect" => $names[spl_object_id($e->getType())] ?? fail("unnamed effect"),
		"duration" => $e->getDuration(),
		"amplifier" => $e->getAmplifier(),
		"visible" => $e->isVisible(),
		"ambient" => $e->isAmbient(),
	], array_values($list));
}

/** An item form, or null for air (an empty residue). */
function optionalItem(Item $item) : ?array{
	return $item->isNull() ? null : itemType($item);
}

/** What the item type says about itself, apart from its Bedrock form. */
function itemData(Item $item, array $kinds, array $effects) : array{
	return [
		"kind" => $kinds[$item->getTypeId()] ?? null,
		"classes" => classChain($item, Item::class),
		"max_stack" => $item->getMaxStackSize(),
		"max_durability" => $item instanceof Durable ? $item->getMaxDurability() : null,
		"fire_proof" => $item->isFireProof(),
		"attack_points" => $item->getAttackPoints(),
		"defense_points" => $item->getDefensePoints(),
		"tool" => [
			"type" => $item->getBlockToolType(),
			"harvest_level" => $item->getBlockToolHarvestLevel(),
			"efficiency" => $item->getMiningEfficiency(true),
		],
		// Defence points are the item's own value above.
		"armor" => $item instanceof Armor ? [
			"slot" => $item->getArmorSlot(),
			// The dump source keeps toughness in the private ArmorTypeInfo and does not use it.
			"toughness" => \Closure::bind(fn(Armor $a) => $a->armorInfo->getToughness(), null, Armor::class)($item),
		] : null,
		"food" => $item instanceof FoodSource ? [
			"food" => $item->getFoodRestore(),
			"saturation" => $item->getSaturationRestore(),
			"requires_hunger" => $item->requiresHunger(),
		] : null,
		"consumable" => $item instanceof Consumable ? [
			"residue" => $item instanceof ConsumableItem ? optionalItem($item->getResidue()) : null,
			"effects" => deterministic(fn() => effectList($item->getAdditionalEffects(), $effects)),
		] : null,
		"cooldown" => $item->getCooldownTicks() > 0 ? ["ticks" => $item->getCooldownTicks(), "tag" => $item->getCooldownTag()] : null,
		"throw_force" => $item instanceof ProjectileItem ? (fn() => $this->getThrowForce())->call($item) : null,
		"potion" => $item instanceof Potion || $item instanceof SplashPotion ? strtolower($item->getType()->name) : null,
		// Item::getBlock: the block state placing the item puts down, clicking the top of a
		// block, and clicking a side where that differs (wall signs, torches, banners).
		"places" => placedBlock($item->getBlock()),
		"places_on_wall" => $item->getBlock(Facing::NORTH)->getStateId() === $item->getBlock()->getStateId() ? null : placedBlock($item->getBlock(Facing::NORTH)),
	];
}

/** The Bedrock form of a block an item places, or null for air (the item places nothing). */
function placedBlock(Block $block) : ?array{
	return $block->getTypeId() === BlockTypeIds::AIR ? null : blockState(GlobalBlockStateHandlers::getSerializer()->serialize($block->getStateId()));
}

$items = [];
$addItem = function(Item $item, ?string $name, bool $vanilla = false) use (&$items, $itemKinds, $effectNames) : void{
	try{
		$type = itemType($item);
	}catch(\Throwable $e){
		// Items without a Bedrock form (none expected) would be unusable on the network.
		fail("item " . $item->getName() . " has no Bedrock form: " . $e->getMessage());
	}
	$k = jsonKey($type);
	$data = itemData($item, $itemKinds, $effectNames);
	$items[$k] ??= ["bedrock" => $type, "data" => $data, "names" => [], "vanilla" => false];
	// Whether VanillaItems gives this form (its default for the kind).
	$items[$k]["vanilla"] = $items[$k]["vanilla"] || $vanilla;
	if(oneLine($items[$k]["data"]) !== oneLine($data)){
		fail("items of form $k disagree on their data");
	}
	if($name !== null && !in_array($name, $items[$k]["names"], true)){
		$items[$k]["names"][] = $name;
	}
};
foreach(VanillaItems::getAll() as $name => $item){
	// AIR is the empty stack, not an item type.
	if(!$item->isNull()){
		$addItem($item, strtolower($name), true);
	}
}
foreach(RuntimeBlockStateRegistry::getInstance()->getAllKnownStates() as $block){
	$item = $block->asItem();
	if(!$item->isNull()){
		$addItem($item, null);
	}
}
$parser = StringToItemParser::getInstance();
foreach($parser->getKnownAliases() as $alias){
	$item = $parser->parse($alias) ?? fail("alias $alias does not parse");
	if(!$item->isNull()){
		$addItem($item, $alias);
	}
}
$itemList = array_values($items);
foreach($itemList as &$entry){
	sort($entry["names"], SORT_STRING);
}
unset($entry);
write($out, "items.json", lines(sortByJson($itemList)));

// Creative inventory, in the dump source's order.
$creative = [];
foreach(CreativeInventory::getInstance()->getAllEntries() as $entry){
	$group = $entry->getGroup();
	$groupName = $group?->getName();
	$creative[] = [
		"category" => match($entry->getCategory()){
			CreativeCategory::CONSTRUCTION => "construction",
			CreativeCategory::NATURE => "nature",
			CreativeCategory::EQUIPMENT => "equipment",
			CreativeCategory::ITEMS => "items",
		},
		"group" => $group === null ? null : [
			"name" => $groupName instanceof Translatable ? $groupName->getText() : $groupName,
			"icon" => itemStack($group->getIcon()),
		],
		"item" => itemStack($entry->getItem()),
	];
}
write($out, "creative.json", lines($creative));

// Tables: effects, potion types, enchantments and banner patterns.
/** The text of a name, which is a translation key or a plain string. */
function text(Translatable|string $name) : string{
	return $name instanceof Translatable ? $name->getText() : $name;
}

function colour(Color $c) : array{
	return ["r" => $c->getR(), "g" => $c->getG(), "b" => $c->getB(), "a" => $c->getA()];
}

$effects = [];
foreach(VanillaEffects::getAll() as $name => $effect){
	$effects[] = [
		"name" => strtolower($name),
		"id" => EffectIdMap::getInstance()->toId($effect),
		"text" => text($effect->getName()),
		"classes" => classChain($effect, Effect::class),
		"colour" => colour($effect->getColor()),
		"bad" => $effect->isBad(),
		"instant" => $effect instanceof InstantEffect,
		"default_duration" => $effect->getDefaultDuration(),
		"bubbles" => $effect->hasBubbles(),
	];
}
$potions = [];
foreach(PotionType::cases() as $type){
	$potions[] = [
		"name" => strtolower($type->name),
		"id" => PotionTypeIdMap::getInstance()->toId($type),
		"text" => $type->getDisplayName(),
		"effects" => effectList($type->getEffects(), $effectNames),
	];
}
$enchantments = [];
foreach(VanillaEnchantments::getAll() as $name => $enchantment){
	$enchantments[] = [
		"name" => strtolower($name),
		"id" => EnchantmentIdMap::getInstance()->toId($enchantment),
	];
}
$banners = [];
foreach(BannerPatternType::cases() as $type){
	$banners[] = ["name" => strtolower($type->name), "id" => BannerPatternTypeIdMap::getInstance()->toId($type)];
}
$tables = [
	"effects" => $effects,
	"potions" => $potions,
	"enchantments" => $enchantments,
	"banner_patterns" => $banners,
];
$tableJson = [];
foreach($tables as $kind => $list){
	$tableJson[] = oneLine($kind) . ": " . lines(sortByJson($list));
}
write($out, "tables.json", "{\n" . implode(",\n", $tableJson) . "\n}");

fwrite(STDOUT, sprintf(
	"dump: %d block states, %d items, %d creative entries, %d effects, %d potion types, %d enchantments\n",
	count($blocks), count($itemList), count($creative), count($effects), count($potions), count($enchantments)
));
