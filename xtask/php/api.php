<?php

//! PMMP public API inventory: reflects every class-like in PMMP's src/ and generated/ into sorted JSON.

/*
 * Run with the PMMP PHP binary from the pinned PocketMine-MP checkout (`.cache/pmmp`) as the
 * working directory:
 *
 *     php api.php <output file>
 *
 * It finds the class-likes each file under src/ and generated/ (the registries PMMP's codegen
 * writes, such as VanillaBlocks) declares (by its tokens, so files that declare none are skipped),
 * loads them through composer's autoloader and writes, for each one, its kind, parent, interfaces
 * (all, inherited included), docblock tags, file (relative to the checkout), the `@method` names
 * of its docblock, its public constants, enum cases and the public methods it declares itself.
 * Every list is sorted, so the same checkout writes the same bytes.
 */

declare(strict_types=1);

require 'vendor/autoload.php';

function fail(string $message) : never{
	fwrite(STDERR, "api: $message\n");
	exit(1);
}

/** Whether a docblock carries `@$tag`. */
function hasTag(string|false $doc, string $tag) : bool{
	return $doc !== false && preg_match('/@' . $tag . '\b/', $doc) === 1;
}

/** The fully qualified class-like names a PHP file declares. @return string[] */
function declaredNames(string $file) : array{
	$tokens = token_get_all(file_get_contents($file) ?: fail("cannot read $file"));
	$namespace = "";
	$names = [];
	$previous = null;
	for($i = 0, $n = count($tokens); $i < $n; $i++){
		$token = $tokens[$i];
		if(!is_array($token)){
			$previous = $token;
			continue;
		}
		if($token[0] === T_NAMESPACE){
			$namespace = "";
			for($i++; $i < $n && $tokens[$i] !== ";" && $tokens[$i] !== "{"; $i++){
				if(is_array($tokens[$i]) && in_array($tokens[$i][0], [T_NAME_QUALIFIED, T_STRING], true)){
					$namespace = $tokens[$i][1];
				}
			}
			continue;
		}
		// `Foo::class` and `new class` use T_CLASS without declaring a name.
		$declares = in_array($token[0], [T_CLASS, T_INTERFACE, T_TRAIT, T_ENUM], true)
			&& !(is_array($previous) && in_array($previous[0], [T_DOUBLE_COLON, T_NEW], true));
		if($token[0] !== T_WHITESPACE && $token[0] !== T_COMMENT && $token[0] !== T_DOC_COMMENT){
			$previous = $token;
		}
		if($declares){
			for($j = $i + 1; $j < $n && is_array($tokens[$j]) && $tokens[$j][0] === T_WHITESPACE; $j++);
			if($j < $n && is_array($tokens[$j]) && $tokens[$j][0] === T_STRING){
				$names[] = ltrim("$namespace\\" . $tokens[$j][1], "\\");
			}
		}
	}
	return $names;
}

/** A default parameter value as source-like text. */
function defaultText(\ReflectionParameter $p) : string{
	if($p->isDefaultValueConstant()){
		return $p->getDefaultValueConstantName();
	}
	$value = $p->getDefaultValue();
	return match(true){
		is_object($value) => $value instanceof \UnitEnum ? get_class($value) . "::" . $value->name : "new " . get_class($value),
		is_array($value) => json_encode($value, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES),
		$value === null => "null",
		default => var_export($value, true),
	};
}

function parameterText(\ReflectionParameter $p) : string{
	$text = $p->hasType() ? (string) $p->getType() . " " : "";
	$text .= ($p->isPassedByReference() ? "&" : "") . ($p->isVariadic() ? "..." : "") . "$" . $p->getName();
	if($p->isDefaultValueAvailable()){
		$text .= " = " . defaultText($p);
	}
	return $text;
}

function kind(\ReflectionClass $class) : string{
	return match(true){
		$class->isEnum() => "enum",
		$class->isInterface() => "interface",
		$class->isTrait() => "trait",
		$class->isAbstract() => "abstract",
		$class->isFinal() => "final",
		default => "class",
	};
}

/** The names of the `@method` lines of a docblock (magic accessors such as VanillaBlocks::STONE()). @return string[] */
function magicMethods(string|false $doc) : array{
	if($doc === false){
		return [];
	}
	preg_match_all('/@method\s+(?:static\s+)?(?:[^\s(]+\s+)?(\w+)\s*\(/', $doc, $matches);
	$names = array_values(array_unique($matches[1]));
	sort($names, SORT_STRING);
	return $names;
}

function describe(\ReflectionClass $class, string $root) : array{
	$name = $class->getName();
	$doc = $class->getDocComment();
	$interfaces = $class->getInterfaceNames();
	sort($interfaces, SORT_STRING);

	$constants = [];
	$cases = [];
	foreach($class->getReflectionConstants() as $constant){
		if($constant->getDeclaringClass()->getName() !== $name || !$constant->isPublic()){
			continue;
		}
		if($constant->isEnumCase()){
			$cases[] = $constant->getName();
		}else{
			$constants[] = $constant->getName();
		}
	}
	sort($constants, SORT_STRING);
	sort($cases, SORT_STRING);

	$methods = [];
	foreach($class->getMethods(\ReflectionMethod::IS_PUBLIC) as $method){
		if($method->getDeclaringClass()->getName() !== $name){
			continue;
		}
		$methodDoc = $method->getDocComment();
		$methods[] = [
			"name" => $method->getName(),
			"static" => $method->isStatic(),
			"abstract" => $method->isAbstract(),
			"internal" => hasTag($methodDoc, "internal"),
			"deprecated" => hasTag($methodDoc, "deprecated"),
			"params" => implode(", ", array_map(fn($p) => parameterText($p), $method->getParameters())),
			"returns" => $method->hasReturnType() ? (string) $method->getReturnType() : "",
		];
	}
	usort($methods, fn(array $a, array $b) => strcmp($a["name"], $b["name"]));

	$magic = magicMethods($doc);
	$parent = $class->getParentClass();
	return [
		"name" => $name,
		"kind" => kind($class),
		"parent" => $parent === false ? null : $parent->getName(),
		"interfaces" => $interfaces,
		"internal" => hasTag($doc, "internal"),
		"deprecated" => hasTag($doc, "deprecated"),
		"file" => str_replace("\\", "/", substr($class->getFileName() ?: "", strlen($root) + 1)),
		"magic_method_count" => count($magic),
		"magic_methods" => $magic,
		"constants" => $constants,
		"cases" => $cases,
		"methods" => $methods,
	];
}

$out = $argv[1] ?? fail("usage: php api.php <output file>");
$root = realpath(".") ?: fail("cannot resolve the working directory");

$files = [];
foreach(["src", "generated"] as $dir){
	is_dir("$root/$dir") || fail("run from the PocketMine-MP checkout (no $dir/ here)");
	foreach(new \RecursiveIteratorIterator(new \RecursiveDirectoryIterator("$root/$dir", \FilesystemIterator::SKIP_DOTS)) as $file){
		if($file->getExtension() === "php"){
			$files[] = $file->getPathname();
		}
	}
}
sort($files, SORT_STRING);

$classes = [];
$failed = [];
foreach($files as $file){
	$relative = str_replace("\\", "/", substr($file, strlen($root) + 1));
	$names = declaredNames($file);
	if(count($names) === 0){
		fwrite(STDERR, "api: skipped $relative (declares no class-like)\n");
		continue;
	}
	foreach($names as $name){
		try{
			$loaded = class_exists($name) || interface_exists($name) || trait_exists($name) || enum_exists($name);
		}catch(\Throwable $e){
			$loaded = false;
		}
		if(!$loaded){
			$failed[] = "$relative: $name";
			continue;
		}
		$class = new \ReflectionClass($name);
		if(!str_starts_with($class->getName(), "pocketmine\\")){
			continue;
		}
		$classes[$class->getName()] = describe($class, $root);
	}
}
ksort($classes, SORT_STRING);

foreach($failed as $line){
	fwrite(STDERR, "api: cannot load $line\n");
}
$json = json_encode(array_values($classes), JSON_THROW_ON_ERROR | JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE);
if(file_put_contents($out, $json . "\n") === false){
	fail("cannot write $out");
}
fwrite(STDOUT, sprintf("api: %d class-likes from %d files, %d failed to load\n", count($classes), count($files), count($failed)));
