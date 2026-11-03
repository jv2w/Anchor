//! `cargo xtask table [--check]`: the API table. Rows are the
//! non-`@internal` class-likes of `xtask/data/dump/api.json` and the non-`@internal` public
//! methods each declares. A row is implemented when an item in `src/` carries its
//! `#[doc(alias)]` (or, for a type named like its API class, a `// api-class: Name` line: see
//! [`scan`]); the status file gives the others a status and implemented rows a reason they
//! differ; `// api:` comments above tests name the aliases each test covers. The table goes to
//! `xtask/data/dump/table.md`; `--check` compares instead of writing.
//!
//! The names of the rows have no namespace root: the tool drops the leading namespace root of the
//! dump file when it reads it.
//!
//! Alias spelling: `Class` or `Class::member`, `Class` being the API short name, or its
//! namespace path (`block\tile\Chest::getPair`) when the short name is ambiguous among
//! non-`@internal` class-likes. The member is a method the class itself declares (a row) or one
//! of its constants or enum cases (valid, but not a row).

mod scan;
mod status;

pub(crate) use scan::is_ident;

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::path::Path;

use serde::Deserialize;

use crate::Mode;
use crate::dump::{file_names, is_dir, read, read_if_exists, write};
use status::{Kind, Rule};

/// Where items carry aliases.
const ALIAS_DIRS: &[&str] = &["src"];
/// The API inventory, relative to the repository root.
const API_PATH: &str = "xtask/data/dump/api.json";
/// The namespace root every class-like of the inventory is in; the tool drops it.
const ROOT: &str = "pocketmine\\";
/// The section of the class-likes that have no namespace below the root.
const ROOT_SECTION: &str = "root";
/// The status file.
pub(crate) const STATUS_PATH: &str = "xtask/data/dump/status.txt";
/// The generated table.
const TABLE_PATH: &str = "xtask/data/dump/table.md";
/// Where tests carry `// api:` comments.
const TEST_DIRS: &[&str] = &["src", "tests", "xtask/tests", "examples"];

/// What a run found.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) counts: Counts,
    /// `path:line: message` for every malformed or unknown alias, comment or status line.
    pub(crate) errors: Vec<String>,
    /// With `--check`: the committed table differs from the generated one.
    pub(crate) stale: bool,
}

/// Rows per status.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Counts {
    pub(crate) implemented: usize,
    pub(crate) differs: usize,
    pub(crate) merged: usize,
    pub(crate) skipped: usize,
    pub(crate) blank: usize,
}

impl Counts {
    fn total(self) -> usize {
        self.implemented + self.differs + self.merged + self.skipped + self.blank
    }

    fn add(&mut self, status: Status) {
        match status {
            Status::Implemented => self.implemented += 1,
            Status::Differs => self.differs += 1,
            Status::Merged => self.merged += 1,
            Status::Skipped => self.skipped += 1,
            Status::Blank => self.blank += 1,
        }
    }
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "implemented {}, differs {}, merged {}, skipped {}, blank {} (of {} rows)",
            self.implemented,
            self.differs,
            self.merged,
            self.skipped,
            self.blank,
            self.total()
        )
    }
}

/// A row's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Implemented,
    Differs,
    Merged,
    Skipped,
    Blank,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Implemented => "implemented",
            Self::Differs => "differs",
            Self::Merged => "merged",
            Self::Skipped => "skipped",
            Self::Blank => "blank",
        }
    }
}

/// A class-like of `api.json` (the fields the table reads).
#[derive(Debug, Deserialize)]
struct Class {
    name: String,
    internal: bool,
    parent: Option<String>,
    interfaces: Vec<String>,
    constants: Vec<String>,
    cases: Vec<String>,
    magic_methods: Vec<String>,
    methods: Vec<Method>,
}

#[derive(Debug, Deserialize)]
struct Method {
    name: String,
    internal: bool,
}

/// Where an alias is in `src/`.
#[derive(Debug)]
struct Location {
    path: String,
    line: usize,
    item: String,
}

/// One table row.
struct Row {
    fqn: String,
    /// The alias that names it in Anchor.
    alias: String,
}

/// The class-likes by name.
struct Index<'a> {
    by_fqn: BTreeMap<&'a str, &'a Class>,
    /// Non-`@internal` class-likes by short name.
    by_short: BTreeMap<&'a str, Vec<&'a Class>>,
    api: &'a [Class],
}

impl<'a> Index<'a> {
    fn new(api: &'a [Class]) -> Self {
        let mut by_short: BTreeMap<&str, Vec<&Class>> = BTreeMap::new();
        for class in api.iter().filter(|c| !c.internal) {
            by_short.entry(short(&class.name)).or_default().push(class);
        }
        Self {
            by_fqn: api.iter().map(|c| (c.name.as_str(), c)).collect(),
            by_short,
            api,
        }
    }

    /// The rows, sorted by FQN.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for class in self.api.iter().filter(|c| !c.internal) {
            let alias = self.class_alias(class);
            for method in class.methods.iter().filter(|m| !m.internal) {
                rows.push(Row {
                    fqn: format!("{}::{}", class.name, method.name),
                    alias: format!("{alias}::{}", method.name),
                });
            }
            rows.push(Row {
                fqn: class.name.clone(),
                alias,
            });
        }
        rows.sort_unstable_by(|a, b| a.fqn.cmp(&b.fqn));
        rows
    }

    /// Checks that `alias` is spelled as the table expects and names an API item.
    fn check(&self, alias: &str) -> Result<(), String> {
        let (class_name, member) = match alias.split_once("::") {
            Some((class, member)) => (class, Some(member)),
            None => (alias, None),
        };
        let class = self.class(class_name)?;
        let Some(member) = member else {
            return Ok(());
        };
        if let Some(method) = class.methods.iter().find(|m| m.name == member) {
            return if method.internal {
                Err(format!("`{}::{member}` is @internal", class.name))
            } else {
                Ok(())
            };
        }
        let is_constant = [&class.constants, &class.cases, &class.magic_methods]
            .iter()
            .any(|names| names.iter().any(|n| n == member));
        if is_constant {
            return Ok(());
        }
        let ancestors = class.parent.iter().chain(&class.interfaces);
        for ancestor in ancestors.filter_map(|name| self.by_fqn.get(name.as_str())) {
            let declares = ancestor
                .methods
                .iter()
                .any(|m| m.name == member && !m.internal);
            if declares && !ancestor.internal {
                return Err(format!(
                    "`{member}` is declared in `{}`: write `{}::{member}`",
                    ancestor.name,
                    self.class_alias(ancestor)
                ));
            }
        }
        Err(format!(
            "`{}` declares no public method, constant or case `{member}`",
            class.name
        ))
    }

    /// The class-like an alias's class part names.
    fn class(&self, name: &str) -> Result<&'a Class, String> {
        if name.contains('\\') {
            let class = self
                .by_fqn
                .get(name)
                .ok_or_else(|| format!("no class-like `{name}`"))?;
            if class.internal {
                return Err(format!("`{name}` is @internal"));
            }
            let short = short(name);
            if self.by_short.get(short).is_some_and(|c| c.len() == 1) {
                return Err(format!("`{short}` is unambiguous: write `{short}`"));
            }
            return Ok(class);
        }
        match self.by_short.get(name).map(Vec::as_slice) {
            Some([class]) => Ok(class),
            Some(classes) => {
                let names: Vec<String> = classes
                    .iter()
                    .map(|c| format!("`{}`", self.class_alias(c)))
                    .collect();
                Err(format!(
                    "`{name}` is ambiguous: write one of {}",
                    names.join(", ")
                ))
            }
            None if self.api.iter().any(|c| short(&c.name) == name) => {
                Err(format!("`{name}` is @internal"))
            }
            None => Err(format!("no class-like `{name}`")),
        }
    }

    /// How aliases name `class`.
    fn class_alias(&self, class: &Class) -> String {
        let short = short(&class.name);
        if self.by_short.get(short).is_some_and(|c| c.len() > 1) {
            class.name.clone()
        } else {
            short.to_owned()
        }
    }
}

/// A row's cells after its status is decided.
struct Judged {
    status: Status,
    locations: String,
    tests: String,
    note: String,
}

/// The rows of one namespace, rendered.
#[derive(Default)]
struct Section {
    counts: Counts,
    lines: String,
}

impl Section {
    fn push(&mut self, row: &Row, judged: &Judged) -> Result<(), String> {
        self.counts.add(judged.status);
        writeln!(
            self.lines,
            "| `{}` | {} | {} | {} | {} |",
            row.fqn,
            judged.status.name(),
            cell(&judged.locations),
            cell(&judged.tests),
            cell(&judged.note)
        )
        .map_err(|e| e.to_string())
    }
}

/// Runs `table`; in check mode, compares instead of writing.
pub(crate) fn run(root: &Path, mode: Mode) -> Result<Report, String> {
    let api_path = root.join(API_PATH);
    let mut api: Vec<Class> =
        serde_json::from_slice(&read(&api_path)?).map_err(|e| format!("{API_PATH}: {e}"))?;
    for class in &mut api {
        class.name = without_root(&class.name);
        class.parent = class.parent.as_deref().map(without_root);
        class.interfaces = class.interfaces.iter().map(|n| without_root(n)).collect();
    }
    let index = Index::new(&api);
    let mut report = Report::default();

    let mut code: BTreeMap<String, Vec<Location>> = BTreeMap::new();
    for (path, text) in sources(root, ALIAS_DIRS)? {
        let (found, errors) = scan::aliases(&text);
        push_errors(&mut report.errors, &path, errors);
        for alias in found {
            match index.check(&alias.name) {
                Ok(()) => code.entry(alias.name).or_default().push(Location {
                    path: path.clone(),
                    line: alias.line,
                    item: alias.item,
                }),
                Err(e) => report.errors.push(format!("{path}:{}: {e}", alias.line)),
            }
        }
    }

    let mut tests: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in sources(root, TEST_DIRS)? {
        let (found, errors) = scan::covered(&text);
        push_errors(&mut report.errors, &path, errors);
        for covered in found {
            for alias in covered.aliases {
                if code.contains_key(&alias) {
                    tests
                        .entry(alias)
                        .or_default()
                        .push(format!("{path}::{}", covered.test));
                } else {
                    report.errors.push(format!(
                        "{path}:{}: `{alias}` is not an alias of any item in src/",
                        covered.line
                    ));
                }
            }
        }
    }
    for list in tests.values_mut() {
        list.dedup();
    }

    let status_text = String::from_utf8(read(&root.join(STATUS_PATH))?)
        .map_err(|e| format!("{STATUS_PATH}: {e}"))?;
    let (rules, errors) = status::parse(&status_text);
    push_errors(&mut report.errors, STATUS_PATH, errors);
    // One error per missing target, at its first rule: a target the code lacks is one thing
    // to add however many rules name it.
    let mut missing: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for rule in &rules {
        if let Kind::Merged(target) = &rule.kind
            && !code.contains_key(target)
        {
            missing.entry(target).or_insert((rule.line, 0)).1 += 1;
        }
    }
    let mut missing: Vec<_> = missing.into_iter().collect();
    missing.sort_unstable_by_key(|(_, (line, _))| *line);
    for (target, (line, count)) in missing {
        let others = match count {
            1 => String::new(),
            2 => " (and 1 more rule)".to_owned(),
            n => format!(" (and {} more rules)", n - 1),
        };
        report.errors.push(format!(
            "{STATUS_PATH}:{line}: merge target `{target}` is not an alias of any item in \
             src/{others}"
        ));
    }

    let mut used = vec![false; rules.len()];
    let mut sections: BTreeMap<&str, Section> = BTreeMap::new();
    let rows = index.rows();
    for row in &rows {
        let line = judge(row, &rules, &mut used, &code, &tests);
        report.counts.add(line.status);
        sections
            .entry(namespace(&row.fqn))
            .or_default()
            .push(row, &line)?;
    }
    for (rule, used) in rules.iter().zip(used) {
        if !used {
            report.errors.push(format!(
                "{STATUS_PATH}:{}: `{}` decides no row",
                rule.line, rule.pattern
            ));
        }
    }

    let table = render(report.counts, &sections)?;
    let table_path = root.join(TABLE_PATH);
    if mode == Mode::Check {
        report.stale = read_if_exists(&table_path)?.as_deref() != Some(table.as_bytes());
    } else {
        write(&table_path, table.as_bytes())?;
    }
    Ok(report)
}

fn push_errors(out: &mut Vec<String>, path: &str, errors: Vec<(usize, String)>) {
    out.extend(
        errors
            .into_iter()
            .map(|(line, e)| format!("{path}:{line}: {e}")),
    );
}

/// The last segment of a class-like name.
fn short(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

/// A dump name without its namespace root.
fn without_root(name: &str) -> String {
    name.strip_prefix(ROOT).unwrap_or(name).to_owned()
}

/// The section of a row: its first namespace (`block`), or [`ROOT_SECTION`] for a class-like
/// with none.
fn namespace(fqn: &str) -> &str {
    let class = fqn.split("::").next().unwrap_or(fqn);
    class
        .split_once('\\')
        .map_or(ROOT_SECTION, |(first, _)| first)
}

/// Decides a row's status: its alias in code makes it implemented (or differs, by the first
/// `differs` rule matching it); otherwise the first rule matching it decides, an `open` rule
/// keeping it blank with a note. Rows with a location but no test are blank.
fn judge(
    row: &Row,
    rules: &[Rule],
    used: &mut [bool],
    code: &BTreeMap<String, Vec<Location>>,
    tests: &BTreeMap<String, Vec<String>>,
) -> Judged {
    let mut first = |accepts: fn(&Kind) -> bool| {
        let found = rules
            .iter()
            .zip(used.iter_mut())
            .find(|(rule, _)| accepts(&rule.kind) && status::matches(&rule.pattern, &row.fqn));
        found.map(|(rule, used)| {
            *used = true;
            &rule.kind
        })
    };
    let (status, target, note) = if code.contains_key(&row.alias) {
        match first(|kind| matches!(kind, Kind::Differs(_))) {
            Some(Kind::Differs(reason)) => (Status::Differs, Some(&row.alias), reason.clone()),
            _ => (Status::Implemented, Some(&row.alias), String::new()),
        }
    } else {
        match first(|_| true) {
            Some(Kind::Differs(reason)) => (
                Status::Blank,
                None,
                format!("differs without an alias: {reason}"),
            ),
            Some(Kind::Merged(target)) if code.contains_key(target) => {
                (Status::Merged, Some(target), format!("into `{target}`"))
            }
            Some(Kind::Merged(target)) => (
                Status::Blank,
                None,
                format!("merge target `{target}` is not in src/"),
            ),
            Some(Kind::Skipped(reason)) => (Status::Skipped, None, reason.clone()),
            Some(Kind::Open(note)) => (Status::Blank, None, format!("open: {note}")),
            None => (Status::Blank, None, String::new()),
        }
    };
    let Some(target) = target else {
        return Judged {
            status,
            locations: String::new(),
            tests: String::new(),
            note,
        };
    };
    let locations: Vec<String> = code
        .get(target)
        .into_iter()
        .flatten()
        .map(|l| format!("{}:{} `{}`", l.path, l.line, l.item))
        .collect();
    let covering = tests.get(target).map_or(&[][..], Vec::as_slice);
    let (status, note) = if covering.is_empty() {
        let was = status.name();
        let note = if note.is_empty() {
            format!("no `// api:` test ({was})")
        } else {
            format!("no `// api:` test ({was}: {note})")
        };
        (Status::Blank, note)
    } else {
        (status, note)
    };
    Judged {
        status,
        locations: locations.join("<br>"),
        tests: covering.join("<br>"),
        note,
    }
}

/// Escapes a Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn render(counts: Counts, sections: &BTreeMap<&str, Section>) -> Result<String, String> {
    let mut out = String::with_capacity(2 << 20);
    out.push_str(
        "<!-- @generated by cargo xtask table; do not edit. -->\n\
         # API table\n\n\
         Rows: the non-`@internal` class-likes of `xtask/data/dump/api.json` and the public \
         non-`@internal` methods each declares. Status: `implemented` (an item in `src/` has the \
         row's `#[doc(alias)]`), `differs` (alias plus a reason in `status.txt`), `merged` (done \
         by the item of another alias, from `status.txt`), `skipped` (reason in `status.txt`), \
         `blank` (none of these, a known gap marked `open` in `status.txt`, or no `// api:` test \
         covers the alias).\n\n\
         | status | rows |\n|---|---:|\n",
    );
    for (name, count) in [
        ("implemented", counts.implemented),
        ("differs", counts.differs),
        ("merged", counts.merged),
        ("skipped", counts.skipped),
        ("blank", counts.blank),
        ("total", counts.total()),
    ] {
        writeln!(out, "| {name} | {count} |").map_err(|e| e.to_string())?;
    }
    for (name, section) in sections {
        write!(
            out,
            "\n## `{name}`\n\n{}\n\n| API | status | Anchor | tests | target / reason |\n\
             |---|---|---|---|---|\n",
            section.counts
        )
        .map_err(|e| e.to_string())?;
        out.push_str(&section.lines);
    }
    Ok(out)
}

/// The `.rs` files under `dirs` (missing directories are skipped) as (path from the root with
/// `/`, text), sorted by path.
pub(crate) fn sources(root: &Path, dirs: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut paths = Vec::new();
    for dir in dirs {
        walk(root, dir, &mut paths)?;
    }
    paths.sort_unstable();
    paths
        .into_iter()
        .map(|path| {
            let bytes = read(&root.join(&path))?;
            let text = String::from_utf8(bytes).map_err(|e| format!("{path}: {e}"))?;
            Ok((path, text))
        })
        .collect()
}

fn walk(root: &Path, dir: &str, out: &mut Vec<String>) -> Result<(), String> {
    let full = root.join(dir);
    if !is_dir(&full) {
        return Ok(());
    }
    for name in file_names(&full)? {
        let path = format!("{dir}/{name}");
        if is_dir(&root.join(&path)) {
            walk(root, &path, out)?;
        } else if Path::new(&name).extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal_class(name: &str) -> Class {
        Class {
            internal: true,
            ..class(name, &[])
        }
    }

    fn class(name: &str, methods: &[(&str, bool)]) -> Class {
        Class {
            name: name.to_owned(),
            internal: false,
            parent: None,
            interfaces: Vec::new(),
            constants: vec!["MAX".to_owned()],
            cases: Vec::new(),
            magic_methods: Vec::new(),
            methods: methods
                .iter()
                .map(|(name, internal)| Method {
                    name: (*name).to_owned(),
                    internal: *internal,
                })
                .collect(),
        }
    }

    fn api() -> Vec<Class> {
        let mut player = class(r"player\Player", &[("getName", false), ("secret", true)]);
        player.parent = Some(r"entity\Human".to_owned());
        vec![
            class(r"entity\Human", &[("getSkin", false)]),
            player,
            class(r"block\Chest", &[("getPair", false)]),
            class(r"block\tile\Chest", &[("getPair", false)]),
            internal_class(r"block\tile\Hidden"),
            internal_class(r"block\Hidden"),
        ]
    }

    #[test]
    fn check_accepts_canonical_aliases_only() {
        let api = api();
        let index = Index::new(&api);
        for good in [
            "Player",
            "Player::getName",
            "Player::MAX",
            r"block\tile\Chest::getPair",
            r"block\Chest",
        ] {
            assert_eq!(index.check(good), Ok(()), "{good}");
        }
        for bad in [
            "Chest::getPair",
            r"player\Player::getName",
            "Player::secret",
            "Player::getSkin",
            "Player::nothing",
            "Hidden",
            "Nobody::x",
            r"block\tile\Nobody",
        ] {
            assert!(index.check(bad).is_err(), "{bad}");
        }
        assert_eq!(
            index.check("Player::getSkin"),
            Err("`getSkin` is declared in `entity\\Human`: write `Human::getSkin`".to_owned())
        );
    }

    #[test]
    fn rows_are_public_classes_and_their_declared_public_methods() {
        let api = api();
        let rows: Vec<(String, String)> = Index::new(&api)
            .rows()
            .into_iter()
            .map(|r| (r.fqn, r.alias))
            .collect();
        let expected = [
            (r"block\Chest", r"block\Chest"),
            (r"block\Chest::getPair", r"block\Chest::getPair"),
            (r"block\tile\Chest", r"block\tile\Chest"),
            (r"block\tile\Chest::getPair", r"block\tile\Chest::getPair"),
            (r"entity\Human", "Human"),
            (r"entity\Human::getSkin", "Human::getSkin"),
            (r"player\Player", "Player"),
            (r"player\Player::getName", "Player::getName"),
        ];
        let expected: Vec<_> = expected
            .iter()
            .map(|(f, a)| ((*f).to_owned(), (*a).to_owned()))
            .collect();
        assert_eq!(rows, expected);
    }

    #[test]
    fn judge_applies_code_first_then_the_first_matching_rule() {
        let row = |fqn: &str, alias: &str| Row {
            fqn: fqn.to_owned(),
            alias: alias.to_owned(),
        };
        let (rules, errors) = status::parse(
            "a\\X::b differs rounds down\n\
             a\\** skipped not here\n\
             b\\Y::c merged X::b\n\
             b\\Y::d merged X::gone\n\
             c\\Z::o open the API is missing\n\
             b\\** differs never reached by code rows\n",
        );
        assert!(errors.is_empty());
        let location = || Location {
            path: "src/x.rs".to_owned(),
            line: 3,
            item: "b".to_owned(),
        };
        let code: BTreeMap<String, Vec<Location>> = [
            ("X::b".to_owned(), vec![location()]),
            ("X::e".to_owned(), vec![location()]),
        ]
        .into_iter()
        .collect();
        let tests: BTreeMap<String, Vec<String>> =
            [("X::b".to_owned(), vec!["src/x.rs::t".to_owned()])]
                .into_iter()
                .collect();
        let mut used = vec![false; rules.len()];
        let mut judged = |fqn: &str, alias: &str| {
            let j = judge(&row(fqn, alias), &rules, &mut used, &code, &tests);
            (j.status, j.note)
        };
        assert_eq!(
            judged(r"a\X::b", "X::b"),
            (Status::Differs, "rounds down".to_owned())
        );
        assert_eq!(
            judged(r"a\X::e", "X::e"),
            (Status::Blank, "no `// api:` test (implemented)".to_owned())
        );
        assert_eq!(
            judged(r"a\X::f", "X::f"),
            (Status::Skipped, "not here".to_owned())
        );
        assert_eq!(
            judged(r"b\Y::c", "Y::c"),
            (Status::Merged, "into `X::b`".to_owned())
        );
        assert_eq!(judged(r"b\Y::d", "Y::d").0, Status::Blank);
        assert_eq!(judged(r"c\Z", "Z"), (Status::Blank, String::new()));
        assert_eq!(
            judged(r"c\Z::o", "Z::o"),
            (Status::Blank, "open: the API is missing".to_owned())
        );
        assert_eq!(used, [true, true, true, true, true, false]);
    }

    #[test]
    fn namespaces_group_rows_by_their_first_namespace() {
        assert_eq!(namespace(r"block\tile\Chest::getPair"), "block");
        assert_eq!(namespace(r"Server::getName"), ROOT_SECTION);
        assert_eq!(namespace(r"Server"), ROOT_SECTION);
    }

    #[test]
    fn the_namespace_root_of_the_dump_is_dropped() {
        assert_eq!(without_root(&format!(r"{ROOT}block\Chest")), r"block\Chest");
        assert_eq!(without_root(r"block\Chest"), r"block\Chest");
    }
}
