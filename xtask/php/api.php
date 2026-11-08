<?php

/*
 * Public API inventory of the dump source, for `cargo xtask table`. Run with the dump PHP binary from the pinned dump source checkout (`.cache/dump`) as the
 * working directory:
 *
 *     php api.php <output file>
 *
 * It finds the class-likes each file under src/ and generated/ (the registries its codegen
 * writes, such as VanillaBlocks) declares (by its tokens, so files that declare none are skipped),
 * loads them through composer's autoloader and writes, for each one, its parent, interfaces
 * (all, inherited included), whether its docblock marks it `@internal`, the `@method` names of
 * its docblock, its public constants, enum cases and the public methods it declares itself (name
 * and `@internal`). Every list is sorted and each class-like is one line, so the same checkout
 * writes the same bytes.
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

function describe(\ReflectionClass $class) : array{
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
		$methods[] = [
			"name" => $method->getName(),
			"internal" => hasTag($method->getDocComment(), "internal"),
		];
	}
	usort($methods, fn(array $a, array $b) => strcmp($a["name"], $b["name"]));

	$parent = $class->getParentClass();
	return [
		"name" => $name,
		"parent" => $parent === false ? null : $parent->getName(),
		"interfaces" => $interfaces,
		"internal" => hasTag($doc, "internal"),
		"magic_methods" => magicMethods($doc),
		"constants" => $constants,
		"cases" => $cases,
		"methods" => $methods,
	];
}

$out = $argv[1] ?? fail("usage: php api.php <output file>");
$root = realpath(".") ?: fail("cannot resolve the working directory");

$files = [];
foreach(["src", "generated"] as $dir){
	is_dir("$root/$dir") || fail("run from the dump source checkout (no $dir/ here)");
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
		$classes[$class->getName()] = describe($class);
	}
}
ksort($classes, SORT_STRING);

foreach($failed as $line){
	fwrite(STDERR, "api: cannot load $line\n");
}
$lines = array_map(fn(array $class) => json_encode($class, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE), array_values($classes));
if(file_put_contents($out, "[\n" . implode(",\n", $lines) . "\n]\n") === false){
	fail("cannot write $out");
}
fwrite(STDOUT, sprintf("api: %d class-likes from %d files, %d failed to load\n", count($classes), count($files), count($failed)));
