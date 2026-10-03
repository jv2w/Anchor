<?php

/*
 * PMMP data dump (PLAN.md 7, 10.4). Run by `cargo xtask dump` with the PMMP PHP binary, from the
 * pinned PocketMine-MP checkout (`.cache/pmmp`) as the working directory:
 *
 *     php dump.php <output directory>
 *
 * It loads PMMP's own registries and writes what they hold as JSON, so the data is what PMMP runs
 * with, not a reading of its source. Every list is sorted, so the same checkout writes the same
 * bytes. Files:
 * - blocks.json: every PMMP block state with its Bedrock serialization, light values, random
 *   ticking and the legacy meta PMMP sends for it as an exact recipe ingredient.
 * - items.json: every PMMP item type with its Bedrock serialization and PMMP names.
 * - creative.json: the creative inventory in order, with its groups.
 * - recipes.json: the recipes PMMP loads from bedrock-data, plus the smithing recipes PMMP leaves
 *   out, read with PMMP's own item reader; recipes naming an item PMMP does not have are left out
 *   as PMMP does.
 *
 * NBT values are written as {"type": value} objects: byte, short, int, long, float, double,
 * string, byte_array, int_array, long_array, list ({"type": element type, "values": [...]}) and
 * compound (keys sorted). Floats are written as their bit patterns so the JSON is exact.
 */

declare(strict_types=1);

require 'vendor/autoload.php';

use pocketmine\block\Block;
use pocketmine\block\RuntimeBlockStateRegistry;
use pocketmine\block\VanillaBlocks;
use pocketmine\crafting\CraftingManagerFromDataHelper;
use pocketmine\crafting\ExactRecipeIngredient;
use pocketmine\crafting\FurnaceType;
use pocketmine\crafting\json\SmithingTransformRecipeData;
use pocketmine\crafting\json\SmithingTrimRecipeData;
use pocketmine\crafting\MetaWildcardRecipeIngredient;
use pocketmine\crafting\RecipeIngredient;
use pocketmine\crafting\ShapelessRecipeType;
use pocketmine\crafting\TagWildcardRecipeIngredient;
use pocketmine\data\bedrock\BedrockDataFiles;
use pocketmine\data\bedrock\block\BlockStateData;
use pocketmine\data\bedrock\item\SavedItemData;
use pocketmine\inventory\CreativeCategory;
use pocketmine\inventory\CreativeInventory;
use pocketmine\item\Item;
use pocketmine\item\StringToItemParser;
use pocketmine\item\VanillaItems;
use pocketmine\lang\Translatable;
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
use pocketmine\network\mcpe\convert\TypeConverter;
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

function ingredient(RecipeIngredient $ingredient) : array{
	return match(true){
		$ingredient instanceof ExactRecipeIngredient => ["exact" => itemStack($ingredient->getItem())],
		$ingredient instanceof MetaWildcardRecipeIngredient => ["any_meta" => $ingredient->getItemId()],
		$ingredient instanceof TagWildcardRecipeIngredient => ["tag" => $ingredient->getTagName()],
		default => fail("unsupported ingredient " . get_class($ingredient)),
	};
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

/** Calls a private static method of PMMP's recipe loader, to read items exactly as PMMP does. */
function helper(string $method, mixed ...$args) : mixed{
	return (fn() => self::$method(...$args))->bindTo(null, CraftingManagerFromDataHelper::class)();
}

$out = $argv[1] ?? fail("usage: php dump.php <output directory>");
if(!is_dir($out)){
	fail("$out is not a directory");
}

// Blocks: every state the runtime registry knows. Names are the VanillaBlocks registry names.
$typeNames = [];
foreach(VanillaBlocks::getAll() as $name => $block){
	$typeNames[$block->getTypeId()] = strtolower($name);
}
$serializer = GlobalBlockStateHandlers::getSerializer();
// PMMP sends an exact block ingredient with the legacy meta of its state (TypeConverter::coreRecipeIngredientToNet).
$translator = TypeConverter::getInstance()->getBlockTranslator();
$blocks = [];
foreach(RuntimeBlockStateRegistry::getInstance()->getAllKnownStates() as $stateId => $block){
	$blocks[] = [
		"type" => $typeNames[$block->getTypeId()] ?? fail("block type {$block->getTypeId()} has no name"),
		"bedrock" => blockState($serializer->serialize($stateId)),
		"light_emission" => $block->getLightLevel(),
		"light_filter" => $block->getLightFilter(),
		"blocks_direct_sky_light" => $block->blocksDirectSkyLight(),
		"ticks_randomly" => $block->ticksRandomly(),
		"recipe_meta" => $translator->getBlockStateDictionary()->getMetaFromStateId($translator->internalIdToNetworkId($stateId)) ?? fail("state $stateId has no meta"),
	];
}
write($out, "blocks.json", lines(sortByJson($blocks)));

// Items: the VanillaItems registry, the item form of every block state, and every name
// StringToItemParser knows, merged by Bedrock form.
$items = [];
$addItem = function(Item $item, ?string $name) use (&$items) : void{
	try{
		$type = itemType($item);
	}catch(\Throwable $e){
		// PMMP items without a Bedrock form (none expected) would be unusable on the network.
		fail("item " . $item->getName() . " has no Bedrock form: " . $e->getMessage());
	}
	$k = jsonKey($type);
	$items[$k] ??= ["bedrock" => $type, "names" => []];
	if($name !== null && !in_array($name, $items[$k]["names"], true)){
		$items[$k]["names"][] = $name;
	}
};
foreach(VanillaItems::getAll() as $name => $item){
	// AIR is the empty stack, not an item type.
	if(!$item->isNull()){
		$addItem($item, strtolower($name));
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

// Creative inventory, in PMMP's order.
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

// Recipes, as PMMP's CraftingManager holds them.
$manager = CraftingManagerFromDataHelper::make(BedrockDataFiles::RECIPES);
$shaped = [];
foreach($manager->getShapedRecipes() as $list){
	foreach($list as $recipe){
		$keys = [];
		foreach($recipe->getIngredientMap() as $y => $row){
			foreach($row as $x => $ingredient){
				$keys[] = ["x" => $x, "y" => $y, "ingredient" => $ingredient === null ? null : ingredient($ingredient)];
			}
		}
		$shaped[] = [
			"width" => $recipe->getWidth(),
			"height" => $recipe->getHeight(),
			"grid" => $keys,
			"results" => array_map(fn(Item $i) => itemStack($i), $recipe->getResults()),
		];
	}
}
$shapeless = [];
foreach($manager->getShapelessRecipes() as $list){
	foreach($list as $recipe){
		$shapeless[] = [
			"block" => match($recipe->getType()){
				ShapelessRecipeType::CRAFTING => "crafting_table",
				ShapelessRecipeType::STONECUTTER => "stonecutter",
				ShapelessRecipeType::SMITHING => "smithing_table",
				ShapelessRecipeType::CARTOGRAPHY => "cartography_table",
			},
			"ingredients" => array_map(fn(RecipeIngredient $i) => ingredient($i), $recipe->getIngredientList()),
			"results" => array_map(fn(Item $i) => itemStack($i), $recipe->getResults()),
		];
	}
}
$furnace = [];
foreach(FurnaceType::cases() as $type){
	foreach($manager->getFurnaceRecipeManager($type)->getAll() as $recipe){
		$furnace[] = [
			"block" => strtolower($type->name),
			"input" => ingredient($recipe->getInput()),
			"result" => itemStack($recipe->getResult()),
		];
	}
}
$potionType = [];
foreach($manager->getPotionTypeRecipes() as $recipe){
	$potionType[] = [
		"input" => ingredient($recipe->getInput()),
		"ingredient" => ingredient($recipe->getIngredient()),
		"output" => itemStack($recipe->getOutput()),
	];
}
$potionContainer = [];
foreach($manager->getPotionContainerChangeRecipes() as $recipe){
	$potionContainer[] = [
		"input" => $recipe->getInputItemId(),
		"ingredient" => ingredient($recipe->getIngredient()),
		"output" => $recipe->getOutputItemId(),
	];
}

// Smithing: PMMP does not load these files ("TODO: smithing"); they are read here with PMMP's
// own item reader and the same rule for unknown items.
$recipesDir = BedrockDataFiles::RECIPES;
$smithingTransform = [];
foreach(CraftingManagerFromDataHelper::loadJsonArrayOfObjectsFile("$recipesDir/smithing.json", SmithingTransformRecipeData::class) as $recipe){
	$template = helper("deserializeIngredient", $recipe->template);
	$input = helper("deserializeIngredient", $recipe->input);
	$addition = helper("deserializeIngredient", $recipe->addition);
	$output = CraftingManagerFromDataHelper::deserializeItemStack($recipe->output);
	if($template === null || $input === null || $addition === null || $output === null){
		continue;
	}
	$smithingTransform[] = [
		"template" => ingredient($template),
		"input" => ingredient($input),
		"addition" => ingredient($addition),
		"result" => itemStack($output),
	];
}
$smithingTrim = [];
foreach(CraftingManagerFromDataHelper::loadJsonArrayOfObjectsFile("$recipesDir/smithing_trim.json", SmithingTrimRecipeData::class) as $recipe){
	$template = helper("deserializeIngredient", $recipe->template);
	$input = helper("deserializeIngredient", $recipe->input);
	$addition = helper("deserializeIngredient", $recipe->addition);
	if($template === null || $input === null || $addition === null){
		continue;
	}
	$smithingTrim[] = [
		"template" => ingredient($template),
		"input" => ingredient($input),
		"addition" => ingredient($addition),
	];
}
$recipeLists = [
	"shaped" => $shaped,
	"shapeless" => $shapeless,
	"furnace" => $furnace,
	"potion_type" => $potionType,
	"potion_container_change" => $potionContainer,
	"smithing_transform" => $smithingTransform,
	"smithing_trim" => $smithingTrim,
];
$recipeJson = [];
foreach($recipeLists as $kind => $list){
	$recipeJson[] = oneLine($kind) . ": " . lines(sortByJson($list));
}
write($out, "recipes.json", "{\n" . implode(",\n", $recipeJson) . "\n}");

fwrite(STDOUT, sprintf(
	"dump: %d block states, %d items, %d creative entries, %d shaped, %d shapeless, %d furnace, %d potion, %d container, %d smithing, %d trim\n",
	count($blocks), count($itemList), count($creative), count($shaped), count($shapeless), count($furnace),
	count($potionType), count($potionContainer), count($smithingTransform), count($smithingTrim)
));
