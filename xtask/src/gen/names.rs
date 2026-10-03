//! Go names to Rust names (CLAUDE.md 5.1, PLAN.md 4). This is the one place for the word
//! splitting rule, the term table and the exception table.
//!
//! Splitting: at each word start the term table is tried first (longest term first; a term
//! must not be followed by a lowercase letter). Otherwise a word is an uppercase run followed
//! by lowercase letters (`Server`), or an uppercase run whose last letter starts the next word
//! when lowercase letters follow (`HTTP|Server`), or a lowercase run. Digits join the word
//! before them (`Vec3`, `Int64`, `V1`). The words are then joined as `snake_case`,
//! `UpperCamelCase` (only the first letter of a word upper) or `SCREAMING_SNAKE_CASE`.

/// Names written together that the case rule would split, and plurals of acronyms that it would
/// split before the `s`.
const TERMS: &[&str] = &["PlayFab", "MoLang", "UUIDs", "XUIDs", "IDs"];

/// Go names, qualified with their package, whose Rust name the rules do not give. Constant groups
/// (`consts`) are named by their Go prefix: `protocol.Container` holds the container IDs
/// (`ContainerAnvilInput`), which the bare name would confuse with containers; gophertunnel
/// misspells `CameraAunAssistPresetOperation`; `WindowIDUI` runs two acronyms together.
const EXCEPTIONS: &[(&str, &str)] = &[
    ("protocol.Container", "ContainerId"),
    ("protocol.WindowIDUI", "WINDOW_ID_UI"),
    (
        "packet.CameraAunAssistPresetOperation",
        "CameraAimAssistPresetOperation",
    ),
];

/// Method names of the extra results of union lookups (Go names them only by position): the
/// lookup function, the result index after the ID, and the method name.
const UNION_EXTRAS: &[(&str, usize, &str)] = &[("protocol.itemDescriptorType", 0, "type_name")];

/// Rust keywords that may be used as raw identifiers.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "trait", "true", "try", "type", "unsafe", "use", "where",
    "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof",
    "unsized", "virtual", "yield",
];

/// Keywords that cannot be raw identifiers.
const RESERVED: &[&str] = &["self", "Self", "super", "crate", "_"];

/// Splits a Go identifier into words.
pub(crate) fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars.get(i) == Some(&'_') {
            i += 1;
            continue;
        }
        if let Some(term) = term_at(&chars, i) {
            words.push(term.to_owned());
            i += term.len();
            continue;
        }
        let start = i;
        let is = |j: usize, f: fn(&char) -> bool| chars.get(j).is_some_and(f);
        if is(i, char::is_ascii_uppercase) {
            let mut j = i;
            while is(j, char::is_ascii_uppercase) {
                j += 1;
            }
            i = if j - start == 1 {
                let mut k = j;
                while is(k, char::is_ascii_lowercase) {
                    k += 1;
                }
                k
            } else if is(j, char::is_ascii_lowercase) {
                j - 1
            } else {
                j
            };
        } else if is(i, char::is_ascii_lowercase) {
            while is(i, char::is_ascii_lowercase) {
                i += 1;
            }
        }
        while is(i, char::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            // Not a letter, digit or underscore: keep it as its own word so the caller fails.
            i += 1;
        }
        words.push(chars.get(start..i).unwrap_or_default().iter().collect());
    }
    words
}

/// The term starting at `i`, if any.
fn term_at(chars: &[char], i: usize) -> Option<&'static str> {
    let mut best: Option<&'static str> = None;
    for &term in TERMS {
        let len = term.chars().count();
        let Some(slice) = chars.get(i..i + len) else {
            continue;
        };
        if slice.iter().copied().eq(term.chars())
            && !chars.get(i + len).is_some_and(char::is_ascii_lowercase)
            && best.is_none_or(|b| b.len() < term.len())
        {
            best = Some(term);
        }
    }
    best
}

/// `snake_case` of `words`.
fn snake_of(words: &[String]) -> String {
    words
        .iter()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("_")
}

/// `UpperCamelCase` of `words`.
fn camel_of(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let lower = w.to_ascii_lowercase();
            let mut chars = lower.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect()
}

/// `SCREAMING_SNAKE_CASE` of `words`.
fn screaming_of(words: &[String]) -> String {
    words
        .iter()
        .map(|w| w.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join("_")
}

/// The exception for a qualified Go name.
fn exception(qualified: &str) -> Option<&'static str> {
    EXCEPTIONS
        .iter()
        .find(|(go, _)| *go == qualified)
        .map(|(_, rust)| *rust)
}

/// Checks that a converted name is a valid identifier, making a keyword raw.
fn identifier(name: String, go: &str) -> Result<String, String> {
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid || RESERVED.contains(&name.as_str()) {
        return Err(format!(
            "`{go}` converts to `{name}`, which is not an identifier; add it to EXCEPTIONS"
        ));
    }
    if KEYWORDS.contains(&name.as_str()) {
        return Ok(format!("r#{name}"));
    }
    Ok(name)
}

/// The Rust field, local or function name of a Go name. `qualified` is the name the
/// exception table uses (for example `protocol.Skin.SkinID`).
pub(crate) fn snake(go: &str, qualified: &str) -> Result<String, String> {
    if let Some(rust) = exception(qualified) {
        return Ok(rust.to_owned());
    }
    identifier(snake_of(&words(go)), qualified)
}

/// The Rust variant name of a Go name: `UpperCamelCase` without the module-word rule of
/// [`type_name`]. `qualified` is the name the exception table uses.
pub(crate) fn camel(go: &str, qualified: &str) -> Result<String, String> {
    if let Some(rust) = exception(qualified) {
        return Ok(rust.to_owned());
    }
    identifier(camel_of(&words(go)), qualified)
}

/// The Rust constant name of a Go name (`SCREAMING_SNAKE_CASE`). `qualified` is the name the
/// exception table uses.
pub(crate) fn screaming(go: &str, qualified: &str) -> Result<String, String> {
    if let Some(rust) = exception(qualified) {
        return Ok(rust.to_owned());
    }
    identifier(screaming_of(&words(go)), qualified)
}

/// The Rust type name of the Go type `qualified` (`package.Name`). Packet types drop a leading
/// or trailing `Packet` word, which would repeat the module name.
pub(crate) fn type_name(qualified: &str) -> Result<String, String> {
    if let Some(rust) = exception(qualified) {
        return Ok(rust.to_owned());
    }
    let (package, go) = qualified.split_once('.').unwrap_or(("", qualified));
    let mut w = words(go);
    if package == "packet" && w.len() > 1 {
        if w.first().is_some_and(|f| f == "Packet") {
            w.remove(0);
        } else if w.last().is_some_and(|l| l == "Packet") {
            w.pop();
        }
    }
    identifier(camel_of(&w), qualified)
}

/// The method name of extra result `k` of the union lookup `union`.
pub(crate) fn union_extra(union: &str, k: usize) -> Result<&'static str, String> {
    UNION_EXTRAS
        .iter()
        .find(|(u, i, _)| *u == union && *i == k)
        .map(|(_, _, name)| *name)
        .ok_or_else(|| format!("{union}: extra result {k} has no method name in UNION_EXTRAS"))
}

/// The variant names of a union: each variant type's words without the words all variants end
/// with (`TakeStackRequestAction` → `Take`). A variant left without words keeps its full name.
pub(crate) fn variants(types: &[&str]) -> Vec<String> {
    let lists: Vec<Vec<String>> = types.iter().map(|t| words(t)).collect();
    let mut common = 0;
    'grow: loop {
        let mut word: Option<&String> = None;
        for list in &lists {
            let Some(w) = list.len().checked_sub(common + 1).and_then(|i| list.get(i)) else {
                break 'grow;
            };
            if list.len() == common + 1 || word.is_some_and(|x| x != w) {
                break 'grow;
            }
            word = Some(w);
        }
        common += 1;
    }
    lists
        .iter()
        .map(|list| camel_of(list.get(..list.len() - common).unwrap_or(list)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(name: &str) -> Vec<String> {
        words(name)
    }

    #[test]
    fn words_split_at_case_boundaries_and_acronyms() {
        assert_eq!(split("EntityRuntimeID"), ["Entity", "Runtime", "ID"]);
        assert_eq!(split("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(split("XBLBroadcastMode"), ["XBL", "Broadcast", "Mode"]);
        assert_eq!(split("IDHashes"), ["ID", "Hashes"]);
        assert_eq!(split("XCoordinates"), ["X", "Coordinates"]);
        assert_eq!(
            split("transferStackRequestAction"),
            ["transfer", "Stack", "Request", "Action"]
        );
        assert_eq!(split("UUID"), ["UUID"]);
    }

    #[test]
    fn digits_join_the_word_before_them() {
        assert_eq!(split("Vec3Value"), ["Vec3", "Value"]);
        assert_eq!(split("Int64"), ["Int64"]);
        assert_eq!(
            split("OnlySpawnV1Villagers"),
            ["Only", "Spawn", "V1", "Villagers"]
        );
        assert_eq!(split("RGBA8"), ["RGBA8"]);
    }

    #[test]
    fn terms_stay_one_word() {
        assert_eq!(split("PlayFabID"), ["PlayFab", "ID"]);
        assert_eq!(
            split("ItemDescriptorMoLang"),
            ["Item", "Descriptor", "MoLang"]
        );
        assert_eq!(split("BlobIDs"), ["Blob", "IDs"]);
        assert_eq!(split("Identity"), ["Identity"]);
    }

    #[test]
    fn names_convert_to_rust_case() {
        assert_eq!(snake("EntityRuntimeID", "x").unwrap(), "entity_runtime_id");
        assert_eq!(snake("XUID", "x").unwrap(), "xuid");
        assert_eq!(snake("NBTData", "x").unwrap(), "nbt_data");
        assert_eq!(snake("PlayFabID", "x").unwrap(), "playfab_id");
        assert_eq!(snake("BlobIDs", "x").unwrap(), "blob_ids");
        assert_eq!(
            type_name("protocol.EntityRuntimeID").unwrap(),
            "EntityRuntimeId"
        );
        assert_eq!(type_name("protocol.UUID").unwrap(), "Uuid");
        assert_eq!(
            type_name("protocol.MoLangItemDescriptor").unwrap(),
            "MolangItemDescriptor"
        );
        assert_eq!(type_name("color.RGBA").unwrap(), "Rgba");
    }

    #[test]
    fn packet_types_drop_the_module_word() {
        assert_eq!(
            type_name("packet.PacketViolationWarning").unwrap(),
            "ViolationWarning"
        );
        assert_eq!(type_name("packet.Text").unwrap(), "Text");
        assert_eq!(type_name("protocol.PacketThing").unwrap(), "PacketThing");
    }

    #[test]
    fn keywords_become_raw_identifiers() {
        assert_eq!(snake("Type", "x").unwrap(), "r#type");
        assert_eq!(snake("Match", "x").unwrap(), "r#match");
        assert!(snake("Self", "x").is_err());
    }

    #[test]
    fn union_variants_drop_the_shared_suffix() {
        assert_eq!(
            variants(&["TakeStackRequestAction", "CraftRecipeStackRequestAction"]),
            ["Take", "CraftRecipe"]
        );
        assert_eq!(variants(&["LastShape", "ArrowShape"]), ["Last", "Arrow"]);
        assert_eq!(variants(&["Shape", "ArrowShape"]), ["Shape", "ArrowShape"]);
    }
}
