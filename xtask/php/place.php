<?php

/*
 * PMMP placement golden table. Run with the PMMP PHP binary from the pinned
 * PocketMine-MP checkout (`.cache/pmmp`) as the working directory:
 *
 *     php place.php <output file>
 *
 * For every item PMMP can place, it runs PMMP's own placement (the two placement transactions of
 * World::useItemOn: the clicked block replaced as if its top was clicked, then the block next to
 * the clicked face, each through Block::canBePlacedAt and Block::place) in a set of small fixed
 * worlds and writes the blocks each transaction would set, as Bedrock states. The interaction
 * tests of Anchor build the same worlds and compare their placement with this table.
 *
 * Scenarios (block positions relative to the clicked block at 0,0,0; the player's feet and yaw
 * are part of each scenario, pitch 0):
 * - floor: the ground fills y 0 for x and z from -2 to 2; the top face of 0,0,0 is clicked
 *   (click y 1 and 0.5); feet at 0.5, 1, 2.5.
 * - wall: the ground at 0,0,0 and stone at y -1 for x and z from -2 to 2; each side face is
 *   clicked (click y 0.25 and 0.75, the other two click coordinates at the face); feet at
 *   0.5, 0, 2.5.
 * - ceiling: the ground at 0,0,0 and stone at y -3 for x and z from -2 to 2; the bottom face is
 *   clicked (click y 0 and 0.5); feet at 0.5, -2, 2.5.
 * - merge: stone at y 0 for x and z from -2 to 2 and, on top, what the item placed in the first
 *   floor scenario on stone; its block at 0,1,0 is clicked on each face (two click heights as
 *   above), yaw 0. Left out if that scenario placed nothing.
 * Every scenario except merge runs with yaws 0, 90, 180, 270 and 30.
 *
 * Output: {"grounds": [...], "scenarios": [...], "states": [...], "results": [...], "items":
 * [{"item": {...}, "results": [...]}]}: a scenario result is an index into "results", whose
 * entries are null (nothing placed) or the transaction's blocks as [dx, dy, dz, state index]
 * sorted by position; states are Bedrock states {name, states}.
 */

declare(strict_types=1);

require 'vendor/autoload.php';

use pocketmine\block\Block;
use pocketmine\block\VanillaBlocks;
use pocketmine\entity\Location;
use pocketmine\item\Item;
use pocketmine\item\StringToItemParser;
use pocketmine\math\Facing;
use pocketmine\math\Vector3;
use pocketmine\nbt\tag\ByteTag;
use pocketmine\nbt\tag\IntTag;
use pocketmine\nbt\tag\StringTag;
use pocketmine\nbt\tag\Tag;
use pocketmine\player\Player;
use pocketmine\world\BlockTransaction;
use pocketmine\world\format\io\GlobalBlockStateHandlers;
use pocketmine\world\format\io\GlobalItemDataHandlers;
use pocketmine\world\World;

function fail(string $message) : never{
	fwrite(STDERR, "place: $message\n");
	exit(1);
}

/** A world of a few blocks: everything else is air. Only what placement reads is implemented. */
final class FakeWorld extends World{
	/** @var Block[] */
	public array $blocks = [];

	public function __construct(){
		//the parent constructor needs a server and a provider; placement needs neither
	}

	public function isLoaded() : bool{
		return true;
	}

	public function isInWorld(int $x, int $y, int $z) : bool{
		return $y >= -64 && $y < 320;
	}

	public function getBlockAt(int $x, int $y, int $z, bool $cached = true, bool $addToCache = true) : Block{
		$block = clone ($this->blocks["$x:$y:$z"] ?? VanillaBlocks::AIR());
		$block->position($this, $x, $y, $z);
		return $block;
	}

	public function getBlock(Vector3 $pos, bool $cached = true, bool $addToCache = true) : Block{
		return $this->getBlockAt((int) floor($pos->x), (int) floor($pos->y), (int) floor($pos->z));
	}

	public function getFullLightAt(int $x, int $y, int $z) : int{
		return 15;
	}

	public function getTile(Vector3 $pos) : ?\pocketmine\block\tile\Tile{
		return null;
	}

	public function getTileAt(int $x, int $y, int $z) : ?\pocketmine\block\tile\Tile{
		return null;
	}

	public function set(int $x, int $y, int $z, Block $block) : void{
		$this->blocks["$x:$y:$z"] = $block;
	}
}

function nbtValue(Tag $tag) : array{
	return match(true){
		$tag instanceof ByteTag => ["byte" => $tag->getValue()],
		$tag instanceof IntTag => ["int" => $tag->getValue()],
		$tag instanceof StringTag => ["string" => $tag->getValue()],
		default => fail("unsupported state tag " . get_class($tag)),
	};
}

function bedrockState(Block $block) : array{
	$data = GlobalBlockStateHandlers::getSerializer()->serializeBlock($block);
	$states = [];
	foreach($data->getStates() as $key => $value){
		$states[$key] = nbtValue($value);
	}
	ksort($states, SORT_STRING);
	return ["name" => $data->getName(), "states" => (object) $states];
}

function itemType(Item $item) : array{
	$data = GlobalItemDataHandlers::getSerializer()->serializeType($item);
	$out = ["name" => $data->getName(), "meta" => $data->getMeta()];
	if($data->getBlock() !== null){
		$states = [];
		foreach($data->getBlock()->getStates() as $key => $value){
			$states[$key] = nbtValue($value);
		}
		ksort($states, SORT_STRING);
		$out["block"] = ["name" => $data->getBlock()->getName(), "states" => (object) $states];
	}
	return $out;
}

/** A player that is never connected or closed: only what placement reads is set. */
final class FakePlayer extends Player{
	public function __destruct(){
		//nothing to close: the constructor never ran
	}
}

/** A player with only what placement reads: position, rotation, size, ID, not sneaking. */
function player(FakeWorld $world, Vector3 $feet, float $yaw) : Player{
	$player = (new \ReflectionClass(FakePlayer::class))->newInstanceWithoutConstructor();
	(function() use ($world, $feet, $yaw) : void{
		$this->location = new Location($feet->x, $feet->y, $feet->z, $world, $yaw, 0.0);
		$this->size = new \pocketmine\entity\EntitySizeInfo(1.8, 0.6, 1.62);
		$this->id = 1;
	})->call($player);
	return $player;
}

/**
 * World::useItemOn's placement: the clicked block replaced as if its top was clicked, else the
 * block next to the clicked face. The transaction's blocks, or null.
 */
function placement(FakeWorld $world, Item $item, Vector3 $clicked, int $face, Vector3 $click, Player $player) : ?array{
	$blockClicked = $world->getBlock($clicked);
	$blockReplace = $blockClicked->getSide($face);
	if($blockClicked->getTypeId() === VanillaBlocks::AIR()->getTypeId()){
		return null;
	}
	$tx = (clone $item)->getPlacementTransaction($blockClicked, $blockClicked, Facing::UP, $click, $player) ??
		(clone $item)->getPlacementTransaction($blockReplace, $blockClicked, $face, $click, $player);
	if($tx === null){
		return null;
	}
	$out = [];
	foreach($tx->getBlocks() as [$x, $y, $z, $block]){
		if(!$world->isInWorld($x, $y, $z)){
			return null;
		}
		$out[] = [$x, $y, $z, $block];
	}
	usort($out, fn($a, $b) => [$a[0], $a[1], $a[2]] <=> [$b[0], $b[1], $b[2]]);
	return $out;
}

$out = $argv[1] ?? fail("usage: php place.php <output file>");

$grounds = [
	"stone" => VanillaBlocks::STONE(),
	"grass" => VanillaBlocks::GRASS(),
	"sand" => VanillaBlocks::SAND(),
	"farmland" => VanillaBlocks::FARMLAND(),
	"soul_sand" => VanillaBlocks::SOUL_SAND(),
	"end_stone" => VanillaBlocks::END_STONE(),
	"clay" => VanillaBlocks::CLAY(),
	"water" => VanillaBlocks::WATER(),
	"jungle_log" => VanillaBlocks::JUNGLE_LOG(),
];
const YAWS = [0.0, 90.0, 180.0, 270.0, 30.0];
const CLICK_Y = [
	Facing::DOWN => [0.0, 0.5],
	Facing::UP => [1.0, 0.5],
	Facing::NORTH => [0.25, 0.75],
	Facing::SOUTH => [0.25, 0.75],
	Facing::WEST => [0.25, 0.75],
	Facing::EAST => [0.25, 0.75],
];

/** The click position on a face of the block at 0,0,0 with height $y. */
function click(int $face, float $y) : Vector3{
	return match($face){
		Facing::NORTH => new Vector3(0.5, $y, 0.0),
		Facing::SOUTH => new Vector3(0.5, $y, 1.0),
		Facing::WEST => new Vector3(0.0, $y, 0.5),
		Facing::EAST => new Vector3(1.0, $y, 0.5),
		default => new Vector3(0.5, $y, 0.5),
	};
}

function layer(FakeWorld $world, int $y, Block $block) : void{
	for($x = -2; $x <= 2; $x++){
		for($z = -2; $z <= 2; $z++){
			$world->set($x, $y, $z, $block);
		}
	}
}

// The scenarios, in output order: [layout, ground, face, yaw, click y, feet].
$scenarios = [];
foreach(array_keys($grounds) as $ground){
	if($ground === "jungle_log"){
		continue;
	}
	foreach(YAWS as $yaw){
		foreach(CLICK_Y[Facing::UP] as $y){
			$scenarios[] = ["floor", $ground, Facing::UP, $yaw, $y, [0.5, 1.0, 2.5]];
		}
	}
}
foreach(["stone", "jungle_log"] as $ground){
	foreach([Facing::NORTH, Facing::SOUTH, Facing::WEST, Facing::EAST] as $face){
		foreach(YAWS as $yaw){
			foreach(CLICK_Y[$face] as $y){
				$scenarios[] = ["wall", $ground, $face, $yaw, $y, [0.5, 0.0, 2.5]];
			}
		}
	}
}
foreach(YAWS as $yaw){
	foreach(CLICK_Y[Facing::DOWN] as $y){
		$scenarios[] = ["ceiling", "stone", Facing::DOWN, $yaw, $y, [0.5, -2.0, 2.5]];
	}
}
foreach([Facing::DOWN, Facing::UP, Facing::NORTH, Facing::SOUTH, Facing::WEST, Facing::EAST] as $face){
	foreach(CLICK_Y[$face] as $y){
		$scenarios[] = ["merge", "stone", $face, 0.0, $y, [0.5, 1.0, 2.5]];
	}
}

/** The world of a scenario; for merge, $first is what the first floor scenario placed. */
function world(array $scenario, array $grounds, ?array $first) : FakeWorld{
	[$layout, $ground] = $scenario;
	$world = new FakeWorld();
	$g = $grounds[$ground];
	match($layout){
		"floor" => layer($world, 0, $g),
		"wall" => (function() use ($world, $g) : void{
			layer($world, -1, VanillaBlocks::STONE());
			$world->set(0, 0, 0, $g);
		})(),
		"ceiling" => (function() use ($world, $g) : void{
			layer($world, -3, VanillaBlocks::STONE());
			$world->set(0, 0, 0, $g);
		})(),
		"merge" => (function() use ($world, $g, $first) : void{
			layer($world, 0, $g);
			foreach($first ?? [] as [$x, $y, $z, $block]){
				$world->set($x, $y, $z, $block);
			}
		})(),
	};
	return $world;
}

// Items: every name StringToItemParser knows, once per Bedrock form, that places a block.
$parser = StringToItemParser::getInstance();
$items = [];
foreach($parser->getKnownAliases() as $alias){
	$item = $parser->parse($alias) ?? fail("alias $alias does not parse");
	if(!$item->canBePlaced()){
		continue;
	}
	$key = json_encode(itemType($item), JSON_THROW_ON_ERROR);
	$items[$key] ??= $item;
}
ksort($items, SORT_STRING);

$stateIndex = [];
$states = [];
$resultIndex = [];
$results = [];
$entries = [];
$scenarioList = [];
foreach($scenarios as [$layout, $ground, $face, $yaw, $y, $feet]){
	$scenarioList[] = ["layout" => $layout, "ground" => $ground, "face" => $face, "yaw" => $yaw, "click_y" => $y, "feet" => $feet];
}
foreach($items as $key => $item){
	$first = null;
	$row = [];
	foreach($scenarios as $i => $scenario){
		[$layout, $ground, $face, $yaw, $y, $feet] = $scenario;
		if($layout === "merge" && $first === null){
			$row[] = 0;
			continue;
		}
		$world = world($scenario, $grounds, $first);
		$clicked = $layout === "merge" ? new Vector3(0, 1, 0) : new Vector3(0, 0, 0);
		$player = player($world, new Vector3(...$feet), $yaw);
		try{
			//vines draw a random age when placed: a fixed seed keeps the table the same every run
			mt_srand(0);
			$tx = placement($world, $item, $clicked, $face, click($face, $y), $player);
		}catch(\Throwable $e){
			fail("$key scenario $i: " . get_class($e) . ": " . $e->getMessage());
		}
		if($i === 0){
			$first = $tx;
		}
		$encoded = null;
		if($tx !== null){
			$encoded = [];
			foreach($tx as [$x, $by, $z, $block]){
				$state = json_encode(bedrockState($block), JSON_THROW_ON_ERROR);
				if(!isset($stateIndex[$state])){
					$stateIndex[$state] = count($states);
					$states[] = bedrockState($block);
				}
				$encoded[] = [$x - (int) $clicked->x, $by - (int) $clicked->y, $z - (int) $clicked->z, $stateIndex[$state]];
			}
		}
		$resultKey = json_encode($encoded, JSON_THROW_ON_ERROR);
		if(!isset($resultIndex[$resultKey])){
			$resultIndex[$resultKey] = count($results);
			$results[] = $encoded;
		}
		$row[] = $resultIndex[$resultKey];
	}
	$entries[] = ["item" => itemType($item), "results" => $row];
}

$groundStates = [];
foreach($grounds as $name => $block){
	$groundStates[$name] = bedrockState($block);
}
$flags = JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_PRESERVE_ZERO_FRACTION;
$lines = [];
$lines[] = '"grounds": ' . json_encode($groundStates, $flags);
$lines[] = '"scenarios": [' . "\n" . implode(",\n", array_map(fn($s) => json_encode($s, $flags), $scenarioList)) . "\n]";
$lines[] = '"states": [' . "\n" . implode(",\n", array_map(fn($s) => json_encode($s, $flags), $states)) . "\n]";
$lines[] = '"results": [' . "\n" . implode(",\n", array_map(fn($r) => json_encode($r, $flags), $results)) . "\n]";
$lines[] = '"items": [' . "\n" . implode(",\n", array_map(fn($e) => json_encode($e, $flags), $entries)) . "\n]";
if(file_put_contents($out, "{\n" . implode(",\n", $lines) . "\n}\n") === false){
	fail("cannot write $out");
}
fwrite(STDERR, "place: " . count($entries) . " items, " . count($scenarios) . " scenarios, " . count($results) . " results\n");
