<?php

/*
 * PMMP block upgrade golden run. Run by `cargo xtask dump` with the PMMP PHP
 * binary, from the pinned PocketMine-MP checkout (`.cache/pmmp`) as the working directory:
 *
 *     php upgrade.php <upgrade dir> <1.12.0.bin> <1.9.0.bin> <reference blocks.json>
 *
 * It builds PMMP's own upgrader (BlockStateUpgrader, BlockIdMetaUpgrader, BlockDataUpgrader) from
 * the schemas the dump copied into `<upgrade dir>/blocks` (BedrockBlockUpgradeSchema 5.4.1), not
 * from the 5.2.0 copy in PMMP's vendor folder (PMMP's legacy ID
 * map class reads only that copy, so its file must equal `<upgrade dir>/block_legacy_id_map.json`),
 * and writes what it gives into `<upgrade dir>/golden/`:
 * - legacy.jsonl: every legacy terrain value, numeric ID 0..255 and meta 0..15, through
 *   upgradeIntIdMeta: {"id", "meta", "state"}, "state" null where PMMP throws.
 * - states.jsonl: upgradeBlockStateNbt over every NBT state of 1.9.0.bin (no version) and of
 *   1.12.0.bin, and over the legacy palette form {name, val} of every string ID of 1.12.0.bin
 *   with meta 0..15: {"in", "out"}, "out" null where PMMP throws.
 * - reference.jsonl: the states of the reference list (the protocol version's states, at their
 *   version) that upgradeBlockStateNbt does not give back unchanged: {"in", "out"}. PMMP applies
 *   every schema of the state's own version when several share it, and from 0321 on all of them
 *   write 1.21.60.33, so 0321 runs again on current states; Anchor skips the schema that
 *   introduced the version, and its test checks it keeps these states.
 *
 * States are written as {"name", "states", "version"} with each property typed ({"byte": n},
 * {"int": n} or {"string": s}, bytes unsigned) and properties sorted, so the same inputs give the
 * same bytes.
 */

declare(strict_types=1);

require 'vendor/autoload.php';

use pmmp\encoding\ByteBufferReader;
use pmmp\encoding\VarInt;
use pocketmine\data\bedrock\block\BlockStateData;
use pocketmine\data\bedrock\block\BlockStateDeserializeException;
use pocketmine\data\bedrock\block\upgrade\BlockDataUpgrader;
use pocketmine\data\bedrock\block\upgrade\BlockIdMetaUpgrader;
use pocketmine\data\bedrock\block\upgrade\BlockStateUpgradeSchemaUtils;
use pocketmine\data\bedrock\block\upgrade\BlockStateUpgrader;
use pocketmine\data\bedrock\block\upgrade\LegacyBlockIdToStringIdMap;
use pocketmine\nbt\LittleEndianNbtSerializer;
use pocketmine\nbt\tag\ByteTag;
use pocketmine\nbt\tag\CompoundTag;
use pocketmine\nbt\tag\IntTag;
use pocketmine\nbt\tag\ShortTag;
use pocketmine\nbt\tag\StringTag;
use pocketmine\nbt\tag\Tag;

[, $dir, $bin112, $bin19, $referencePath] = $argv;

$stateUpgrader = new BlockStateUpgrader(BlockStateUpgradeSchemaUtils::loadSchemas($dir . '/blocks', PHP_INT_MAX));
// PMMP's ID map class reads only its vendor copy (5.2.0); its file must equal the 5.3.0 copy.
if(file_get_contents(\pocketmine\BEDROCK_BLOCK_UPGRADE_SCHEMA_PATH . '/block_legacy_id_map.json') !== file_get_contents($dir . '/block_legacy_id_map.json')){
	fwrite(STDERR, "PMMP's block_legacy_id_map.json differs from the copied one\n");
	exit(1);
}
$idMap = LegacyBlockIdToStringIdMap::getInstance();
$upgrader = new BlockDataUpgrader(
	BlockIdMetaUpgrader::loadFromString(file_get_contents($bin112), $idMap, $stateUpgrader),
	$stateUpgrader
);

function typed(Tag $tag) : array{
	return match(true){
		$tag instanceof ByteTag => ["byte" => $tag->getValue() & 0xff],
		$tag instanceof IntTag => ["int" => $tag->getValue()],
		$tag instanceof StringTag => ["string" => $tag->getValue()],
		$tag instanceof ShortTag => ["short" => $tag->getValue()],
		default => throw new \RuntimeException("unexpected state tag " . get_class($tag))
	};
}

function state(BlockStateData $data) : array{
	$states = [];
	foreach($data->getStates() as $k => $v){
		$states[(string) $k] = typed($v);
	}
	ksort($states, SORT_STRING);
	return ["name" => $data->getName(), "states" => (object) $states, "version" => $data->getVersion()];
}

/** The input as written: name, states typed, version if present; or name and val. */
function input(CompoundTag $tag) : array{
	if($tag->getTag("val") !== null){
		return ["name" => $tag->getString("name"), "val" => $tag->getShort("val")];
	}
	$states = [];
	foreach($tag->getCompoundTag("states")->getValue() as $k => $v){
		$states[(string) $k] = typed($v);
	}
	ksort($states, SORT_STRING);
	$out = ["name" => $tag->getString("name"), "states" => (object) $states];
	if($tag->getTag("version") !== null){
		$out["version"] = $tag->getInt("version");
	}
	return $out;
}

function line(array $value) : string{
	return json_encode($value, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE) . "\n";
}

/** @return list<array{string, int, CompoundTag}> the entries of a legacy table, in file order */
function entries(string $path) : array{
	$reader = new ByteBufferReader(file_get_contents($path));
	$nbt = new LittleEndianNbtSerializer();
	$entries = [];
	$ids = VarInt::readUnsignedInt($reader);
	for($i = 0; $i < $ids; $i++){
		$id = $reader->readByteArray(VarInt::readUnsignedInt($reader));
		$metas = VarInt::readUnsignedInt($reader);
		for($m = 0; $m < $metas; $m++){
			$meta = VarInt::readUnsignedInt($reader);
			$offset = $reader->getOffset();
			$state = $nbt->read($reader->getData(), $offset)->mustGetCompoundTag();
			$reader->setOffset($offset);
			$entries[] = [$id, $meta, $state];
		}
	}
	if($reader->getUnreadLength() > 0){
		throw new \RuntimeException("$path: trailing bytes");
	}
	return $entries;
}

$legacy = "";
for($id = 0; $id < 256; $id++){
	for($meta = 0; $meta < 16; $meta++){
		try{
			$out = state($upgrader->upgradeIntIdMeta($id, $meta));
		}catch(BlockStateDeserializeException){
			$out = null;
		}
		$legacy .= line(["id" => $id, "meta" => $meta, "state" => $out]);
	}
}
file_put_contents($dir . '/golden/legacy.jsonl', $legacy);

$inputs = [];
foreach(entries($bin19) as [, , $state]){
	$inputs[] = $state;
}
$names = [];
foreach(entries($bin112) as [$id, , $state]){
	$inputs[] = $state;
	$names[$id] = true;
}
foreach(array_keys($names) as $id){
	for($meta = 0; $meta < 16; $meta++){
		$inputs[] = CompoundTag::create()->setString("name", (string) $id)->setShort("val", $meta);
	}
}
$states = "";
foreach($inputs as $tag){
	try{
		$out = state($upgrader->upgradeBlockStateNbt($tag));
	}catch(BlockStateDeserializeException){
		$out = null;
	}
	$states .= line(["in" => input($tag), "out" => $out]);
}
file_put_contents($dir . '/golden/states.jsonl', $states);

$changed = "";
foreach(json_decode(file_get_contents($referencePath), true, flags: JSON_THROW_ON_ERROR) as $block){
	$tags = [];
	foreach($block["states"] as $k => $v){
		$tags[$k] = match(array_key_first($v)){
			"byte" => new ByteTag($v["byte"]),
			"int" => new IntTag($v["int"]),
			"string" => new StringTag($v["string"]),
		};
	}
	$in = new BlockStateData($block["name"], $tags, $block["version"]);
	$out = $stateUpgrader->upgrade($in);
	if(json_encode(state($in)) !== json_encode(state($out))){
		$changed .= line(["in" => state($in), "out" => state($out)]);
	}
}
file_put_contents($dir . '/golden/reference.jsonl', $changed);
