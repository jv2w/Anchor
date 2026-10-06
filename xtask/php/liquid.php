<?php
// Liquid flow goldens: PMMP 6a7cc02's own Liquid, Water, Lava and
// MinimumCostFlowCalculator run in a stand-in World whose block storage and update queues copy
// World::actuallyDoTick, scheduleDelayedBlockUpdate and internalNotifyNeighbourBlockUpdate. The
// ground is the flat world: bedrock at y -64, dirt -63 and -62, grass -61. The Anchor test
// `tick::update::tests::liquid_scenes_match_pmmp` runs the same scenes. From the pinned checkout
// (.cache/pmmp) as the working directory, with the PMMP PHP binary:
//
//     php <anchor>/xtask/php/liquid.php > <anchor>/xtask/data/pmmp/liquid.txt
declare(strict_types=1);
require 'vendor/autoload.php';

use pocketmine\block\Block;
use pocketmine\block\RuntimeBlockStateRegistry;
use pocketmine\block\VanillaBlocks;
use pocketmine\item\Item;
use pocketmine\math\Facing;
use pocketmine\math\Vector3;
use pocketmine\player\Player;
use pocketmine\utils\ReversePriorityQueue;
use pocketmine\world\format\Chunk;
use pocketmine\world\sound\Sound;
use pocketmine\world\World;
use pocketmine\world\format\io\GlobalBlockStateHandlers;

final class FakeWorld extends World{
	/** @var int[] */
	public array $blocks = [];
	public int $tick = 0;
	private ReversePriorityQueue $queue;
	private array $queueIndex = [];
	private \SplQueue $neighbours;
	private array $neighbourIndex = [];
	private RuntimeBlockStateRegistry $reg;

	public function __construct(){
		$this->queue = new ReversePriorityQueue();
		$this->queue->setExtractFlags(\SplPriorityQueue::EXTR_BOTH);
		$this->neighbours = new \SplQueue();
		$this->reg = RuntimeBlockStateRegistry::getInstance();
	}

	public function isLoaded() : bool{ return true; }

	public function isInWorld(int $x, int $y, int $z) : bool{
		return $y >= World::Y_MIN && $y < World::Y_MAX;
	}

	private function ground(int $y) : Block{
		return match(true){
			$y === -64 => VanillaBlocks::BEDROCK(),
			$y === -63, $y === -62 => VanillaBlocks::DIRT(),
			$y === -61 => VanillaBlocks::GRASS(),
			default => VanillaBlocks::AIR(),
		};
	}

	public function getBlockAt(int $x, int $y, int $z, bool $cached = true, bool $addToCache = true) : Block{
		if(!$this->isInWorld($x, $y, $z)){
			$block = VanillaBlocks::AIR();
		}else{
			$id = $this->blocks[World::blockHash($x, $y, $z)] ?? null;
			$block = $id === null ? $this->ground($y) : $this->reg->fromStateId($id);
		}
		$block->position($this, $x, $y, $z);
		return $block->readStateFromWorld();
	}

	public function getBlock(Vector3 $pos, bool $cached = true, bool $addToCache = true) : Block{
		return $this->getBlockAt((int) floor($pos->x), (int) floor($pos->y), (int) floor($pos->z));
	}

	public function setBlockAt(int $x, int $y, int $z, Block $block, bool $update = true) : void{
		$this->blocks[World::blockHash($x, $y, $z)] = $block->getStateId();
		if($update){
			$this->notify($x, $y, $z);
		}
	}

	public function setBlock(Vector3 $pos, Block $block, bool $update = true) : void{
		$this->setBlockAt((int) floor($pos->x), (int) floor($pos->y), (int) floor($pos->z), $block, $update);
	}

	public function useBreakOn(Vector3 $vector, ?Item &$item = null, ?Player $player = null, bool $createParticles = false, array &$returnedItems = []) : bool{
		$this->setBlock($vector, VanillaBlocks::AIR());
		return true;
	}

	public function addSound(Vector3 $pos, Sound $sound, ?array $players = null) : void{}

	// World::scheduleDelayedBlockUpdate, with the server tick.
	public function scheduleDelayedBlockUpdate(Vector3 $pos, int $delay) : void{
		if(
			!$this->isInWorld((int) $pos->x, (int) $pos->y, (int) $pos->z) ||
			(isset($this->queueIndex[$index = World::blockHash((int) $pos->x, (int) $pos->y, (int) $pos->z)]) && $this->queueIndex[$index] <= $delay)
		){
			return;
		}
		$this->queueIndex[$index] = $delay;
		$this->queue->insert(new Vector3((int) $pos->x, (int) $pos->y, (int) $pos->z), $delay + $this->tick);
	}

	private function tryAdd(int $x, int $y, int $z) : void{
		if($this->isInWorld($x, $y, $z)){
			$hash = World::blockHash($x, $y, $z);
			if(!isset($this->neighbourIndex[$hash])){
				$this->neighbours->enqueue($hash);
				$this->neighbourIndex[$hash] = true;
			}
		}
	}

	private function notify(int $x, int $y, int $z) : void{
		$this->tryAdd($x, $y, $z);
		foreach(Facing::OFFSET as [$dx, $dy, $dz]){
			$this->tryAdd($x + $dx, $y + $dy, $z + $dz);
		}
	}

	// The update part of World::actuallyDoTick.
	public function step() : void{
		while($this->queue->count() > 0 && $this->queue->current()["priority"] <= $this->tick){
			$vec = $this->queue->extract()["data"];
			unset($this->queueIndex[World::blockHash($vec->x, $vec->y, $vec->z)]);
			$this->getBlock($vec)->onScheduledUpdate();
		}
		while($this->neighbours->count() > 0){
			$index = $this->neighbours->dequeue();
			unset($this->neighbourIndex[$index]);
			World::getBlockXYZ($index, $x, $y, $z);
			$this->getBlockAt($x, $y, $z)->onNearbyBlockChange();
		}
		$this->tick++;
	}
}

function show(Block $b) : string{
	$data = GlobalBlockStateHandlers::getSerializer()->serialize($b->getStateId());
	$states = [];
	foreach($data->getStates() as $k => $v){
		$states[] = $k . "=" . $v->getValue();
	}
	sort($states);
	return $data->getName() . "[" . implode(",", $states) . "]";
}

/** Every block that differs from the ground in the box, one line each. */
function dump(FakeWorld $w, string $scene, int $x0, int $x1, int $y0, int $y1, int $z0, int $z1) : void{
	ksort($w->blocks);
	$lines = [];
	for($y = $y0; $y <= $y1; $y++) for($z = $z0; $z <= $z1; $z++) for($x = $x0; $x <= $x1; $x++){
		$hash = World::blockHash($x, $y, $z);
		if(isset($w->blocks[$hash])){
			$b = $w->getBlockAt($x, $y, $z);
			if($b->getTypeId() !== VanillaBlocks::AIR()->getTypeId()){
				$lines[] = "$scene $x $y $z " . show($b);
			}
		}
	}
	echo implode("\n", $lines), "\n";
}

/** @param array<int, list<array{int,int,int,Block}>> $actions by tick */
function scene(string $name, array $actions, int $ticks, array $box) : void{
	$w = new FakeWorld();
	for($t = 0; $t < $ticks; $t++){
		foreach($actions[$t] ?? [] as [$x, $y, $z, $block]){
			$w->setBlockAt($x, $y, $z, $block);
		}
		$w->step();
	}
	dump($w, $name, ...$box);
}

$water = VanillaBlocks::WATER();
$lava = VanillaBlocks::LAVA();
$stone = VanillaBlocks::STONE();
$air = VanillaBlocks::AIR();
echo "# default water ", show($water), " lava ", show($lava), "\n";

scene("source", [0 => [[8, -60, 8, $water]]], 120, [-2, 18, -61, -55, -2, 18]);

$platform = [];
for($x = 6; $x <= 10; $x++) for($z = 6; $z <= 10; $z++) $platform[] = [$x, -56, $z, $stone];
$platform[] = [8, -55, 8, $water];
scene("waterfall", [0 => $platform], 200, [-2, 18, -61, -50, -2, 18]);

scene("lava_water", [0 => [[4, -60, 8, $water], [12, -60, 8, $lava]]], 600, [-6, 22, -61, -55, -2, 18]);

scene("dry_up", [0 => [[8, -60, 8, $water]], 100 => [[8, -60, 8, $air]]], 300, [-2, 18, -61, -55, -2, 18]);

$steps = [0 => [[8, -60, 8, $water]]];
for($z = 4; $z <= 12; $z++) $steps[0][] = [11, -60, $z, $stone];
$steps[40] = [[11, -60, 8, $air]];
scene("wall_opened", $steps, 160, [-2, 18, -61, -55, -2, 18]);
