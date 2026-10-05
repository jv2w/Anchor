//! The API scenarios of `cargo xtask p6` (PLAN.md 10.8 P6): `commands`, `particles`, `kick`,
//! `forms` and `script` (the bot's `api.go`), and `weather`, `joinrain` and `joinstorm` (the
//! bot's `weather.go`, PLAN.md 10.9 날씨와 번개: each runs after the one before it, whose weather
//! it joins in), and `boltpeaceful`, `bolteasy` and `bolthard` (a bolt next to the bot at that
//! difficulty). They run on the game scenarios' machinery (`game`):
//! the bot plays each one with its own player name and writes the console commands it needs as
//! `command` records, which the harness runs on the server's console. The script scenario runs
//! the actions of `SCRIPT`.
//!
//! - `p6 api bds [scenario]...`: one BDS session (RakNet, zlib, a fresh flat world) with cheats
//!   allowed (player commands need them) and the forms script pack (`FORMS_PACK`) installed in the
//!   new world, into `target/p6/bds-api/`; the session first runs `game::SESSION_SETUP`.
//! - `p6 api anchor [scenario]...`: the same against Anchor in this process (`game::console`
//!   carries out `op`, `kick`, `scriptevent p6:<kind>`, `summon snowball|egg`, `weather`, `difficulty` and
//!   `summon lightning_bolt` through Anchor's public API; player commands are Anchor's own), into
//!   `target/p6/anchor-api/`, then compared with `bds-api/` (also written to
//!   `anchor-api/compare.txt`).
//! - `p6 api compare A B`: compares the API captures of two folders; a missing one is a difference.
//!
//! Each capture gets a report (`<scenario>.txt`): per step of the bot, its output, check, form and
//! command list lines and the packets of `REPORTED` with their fields. The comparison pairs the
//! steps of two captures in order. In each step that is not a setup step, the packets the
//! fixed-field list (`xtask/data/p6/fields.txt`) names must come in the same numbers with the same
//! fixed fields, the bot's output lines must have the same `uuid_echoed` and `state`, its form
//! lines the same form JSON (numbers compared as numbers) and its check lines the same values. The
//! command lists (`available` lines) are compared only for the commands both servers have and only
//! as notes: BDS has vanilla's commands, Anchor PMMP's. The `particles` and `script` scenarios are
//! facts only.
//!
//! Differences Anchor keeps on purpose are listed with their reasons in the intended-difference
//! file (`xtask/data/p6/intended.txt`, reviewed like the deferral table): a difference whose text
//! is exactly one of its lines, in that line's scenario, is reported as `intended` and does not
//! fail the comparison; every other difference does. A listed difference the comparison no longer
//! finds is a note, so the file can be trimmed.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::compare::{self, Received, field};
use super::game;

/// The API scenarios, in run order.
pub(crate) const SCENARIOS: [&str; 45] = [
    "commands",
    "particles",
    "kick",
    "forms",
    "script",
    "weather",
    "joinrain",
    "joinstorm",
    "boltpeaceful",
    "bolteasy",
    "bolthard",
    "enchants",
    "stations",
    "potions",
    "entities",
    "villagers",
    "numbers",
    "syntax",
    "limits",
    "limitchat",
    "line256",
    "line257",
    "line384",
    "line448",
    "line480",
    "line484",
    "line488",
    "line492",
    "line496",
    "line500",
    "line504",
    "line505",
    "line506",
    "line507",
    "line508",
    "line511",
    "lineunknown600",
    "line512",
    "line1000",
    "linekorean387",
    "linekorean519",
    "others",
    "rules",
    "rulesjoin",
    "tradeup",
];
/// The scenarios whose comparison is facts only: the particles scenario records what each server
/// sends, the script scenario tests the bot's `-script` actions (`SCRIPT`), and the M9
/// verification scenarios `villagers` (random professions), `numbers` (BDS-only items and
/// telemetry), `rules` and `rulesjoin` (game rules Anchor takes from its settings, not a command)
/// and the command line lengths (BDS ends the connection from 508 bytes, Anchor answers) record
/// BDS facts for `xtask/data/p6/m9-facts.md`.
const FACTS_ONLY: [&str; 27] = [
    "particles",
    "script",
    "villagers",
    "numbers",
    "rules",
    "rulesjoin",
    "line256",
    "line257",
    "line384",
    "line448",
    "line480",
    "line484",
    "line488",
    "line492",
    "line496",
    "line500",
    "line504",
    "line505",
    "line506",
    "line507",
    "line508",
    "line511",
    "lineunknown600",
    "line512",
    "line1000",
    "linekorean387",
    "linekorean519",
];
/// The M9 verification scenarios that compare their checks only, not the fixed-field packets of
/// their steps: what each checks is its item (potion colours and cloud, another player, chat
/// limits), and the other packets differ for reasons recorded in `xtask/data/p6/m9-facts.md` (a
/// splash's effect durations follow PMMP, Anchor keeps no default death message).
const CHECKS_ONLY: &[&str] = &[
    "potions",
    "others",
    "mover",
    "limits",
    "limitchat",
    "tradeup",
];
/// Scenarios with a second bot (`game::run_scenarios`): the others scenario watches the mover.
const COMPANIONS: &[(&str, &str)] = &[("others", "mover")];
/// The action file of the script scenario, relative to the repository root.
const SCRIPT: &str = "xtask/data/p6/script.txt";
/// The intended-difference file, relative to the repository root.
const INTENDED: &str = "xtask/data/p6/intended.txt";
/// Settings of the BDS session besides the test settings: players may run cheat commands.
const BDS_SETTINGS: &[(&str, &str)] = &[("allow-cheats", "true")];
/// The forms script pack, relative to the repository root, and its files.
const FORMS_PACK: &str = "xtask/data/p6/forms-pack";
const FORMS_PACK_FILES: [&str; 2] = ["manifest.json", "scripts/main.js"];
/// The pack's folder in the BDS copy's `development_behavior_packs`.
const FORMS_PACK_DIR: &str = "p6_forms";
/// The packets each step's report shows with their fields; others are only counted.
const REPORTED: &[&str] = &[
    "ActorEvent",
    "AddActor",
    "CommandOutput",
    "Disconnect",
    "LevelEvent",
    "LevelEventGeneric",
    "LevelSoundEvent",
    "MobEffect",
    "ModalFormRequest",
    "MovePlayer",
    "RemoveActor",
    "SetCommandsEnabled",
    "SetPlayerGameType",
    "SetTime",
    "SetTitle",
    "SpawnParticleEffect",
    "SyncWorldClocks",
    "Text",
    "ToastRequest",
    "UpdateAbilities",
    "UpdatePlayerGameType",
];
/// The bot's checks whose differences are notes: BDS sends every command to every player and
/// leaves hiding them to the client's permission level, so it does not resend the list after
/// `op`; Anchor sends only the commands the player may use (PMMP) and resends the list.
const NOTE_CHECKS: [&str; 1] = ["op resends AvailableCommands"];
/// The packets compared in setup steps too: the console runs a setup step's commands on BDS and
/// the harness calls Anchor's API for them, so their messages differ, but the player's new
/// permissions after `op` must reach the client the same way.
const SETUP_COMPARED: &[&str] = &["UpdateAbilities"];
/// How many characters of a packet's fields a report line shows.
const REPORT_WIDTH: usize = 600;

/// Picks the scenarios named in `args` (all when empty).
fn scenarios<'a>(args: &[&'a str]) -> Result<Vec<&'a str>, String> {
    if args.is_empty() {
        return Ok(SCENARIOS.to_vec());
    }
    args.iter()
        .map(|a| {
            SCENARIOS
                .iter()
                .find(|s| *s == a)
                .map(|_| *a)
                .ok_or_else(|| format!("unknown API scenario `{a}` (want {SCENARIOS:?})"))
        })
        .collect()
}

/// `p6 api bds [scenario]...`: [`session`] alone.
pub(crate) fn bds(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let scenarios = scenarios(args)?;
    let exe = root.join(super::BDS_DIR).join("bedrock_server.exe");
    if !super::is_file(&exe) {
        return Err(format!("{} is missing", exe.display()));
    }
    let bot = super::build_bot(root)?;
    let passed = session(root, &exe, &bot, &scenarios, out)?;
    super::say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// `p6 api anchor [scenario]...`: [`anchor_session`] alone.
pub(crate) fn anchor(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let scenarios = scenarios(args)?;
    let bot = super::build_bot(root)?;
    let passed = anchor_session(root, &bot, &scenarios, out)?;
    super::say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// The BDS session of the API scenarios into `target/p6/bds-api/`, each with its report.
pub(crate) fn session(
    root: &Path,
    exe: &Path,
    bot: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let dir = root.join(super::OUT_DIR).join("bds-api");
    let session = super::BdsSession {
        transport: "raknet",
        compression: "zlib",
        settings: BDS_SETTINGS,
    };
    let pack = root.join(FORMS_PACK);
    let install = |bds: &Path| install_pack(&pack, bds);
    super::bds_session_with(exe, &dir, &session, install, out, |addr, server, out| {
        for line in game::SESSION_SETUP {
            server.command(line)?;
        }
        let mut passed = true;
        for scenario in scenarios {
            let script = script_of(root, scenario);
            let console = |l: &str| server.command(l);
            passed &= game::run_scenarios(
                bot,
                addr,
                &dir,
                scenario,
                script.as_deref(),
                companion(scenario),
                console,
                out,
            )?;
            write_report(&dir, scenario)?;
            if let Some(c) = companion(scenario) {
                write_report(&dir, c)?;
            }
        }
        Ok(passed)
    })
}

/// The companion scenario of `scenario`, if it has one.
fn companion(scenario: &str) -> Option<&'static str> {
    COMPANIONS
        .iter()
        .find(|(s, _)| *s == scenario)
        .map(|(_, c)| *c)
}

/// The action file of the scenario, for the script scenario.
fn script_of(root: &Path, scenario: &str) -> Option<std::path::PathBuf> {
    (scenario == "script").then(|| root.join(SCRIPT))
}

/// Installs the forms pack into the BDS copy `bds` and turns it on for the world it creates
/// (`worlds/<level-name>/world_behavior_packs.json`).
fn install_pack(pack: &Path, bds: &Path) -> Result<(), String> {
    let target = bds.join("development_behavior_packs").join(FORMS_PACK_DIR);
    super::remove_dir(&target)?;
    for file in FORMS_PACK_FILES {
        let to = target.join(file);
        if let Some(parent) = to.parent() {
            super::create_dir(parent)?;
        }
        super::write(&to, super::read_text(&pack.join(file))?.as_bytes())?;
    }
    let manifest: Value = serde_json::from_str(&super::read_text(&pack.join("manifest.json"))?)
        .map_err(|e| format!("{FORMS_PACK}/manifest.json: {e}"))?;
    let header = field(&manifest, "header");
    let entry = serde_json::json!([{
        "pack_id": field(header, "uuid"),
        "version": field(header, "version"),
    }]);
    let level = super::BDS_SETTINGS
        .iter()
        .find(|(key, _)| *key == "level-name")
        .map_or("p6", |(_, value)| value);
    let world = bds.join("worlds").join(level);
    super::create_dir(&world)?;
    super::write(
        &world.join("world_behavior_packs.json"),
        entry.to_string().as_bytes(),
    )
}

/// The API session against Anchor into `target/p6/anchor-api/`, then its comparison with the
/// BDS session's captures. It passes when every bot and console command passed and the captures
/// are the same.
pub(crate) fn anchor_session(
    root: &Path,
    bot: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let dir = root.join(super::OUT_DIR).join("anchor-api");
    let (mut console, task) = game::console::console();
    let with_console = |b: anchor::ServerBuilder| b.every(game::console::EVERY_TICK, task);
    let played = super::anchor_session(&dir, "zlib", None, with_console, out, |addr, _, out| {
        for line in game::SESSION_SETUP {
            console.send(line)?;
        }
        let mut passed = console.settle(out)?;
        for scenario in scenarios {
            let script = script_of(root, scenario);
            let send = |l: &str| console.send(l);
            passed &= game::run_scenarios(
                bot,
                addr,
                &dir,
                scenario,
                script.as_deref(),
                companion(scenario),
                send,
                out,
            )?;
            passed &= console.settle(out)?;
            write_report(&dir, scenario)?;
            if let Some(c) = companion(scenario) {
                write_report(&dir, c)?;
            }
        }
        Ok(passed)
    })?;
    let mut report = Vec::new();
    super::say(&mut report, "compare bds-api anchor-api:")?;
    let bds = root.join(super::OUT_DIR).join("bds-api");
    let same = compare_scenarios(root, &bds, &dir, scenarios, &mut report).or_else(|e| {
        super::say(
            &mut report,
            &format!("DIFFERENT: {e}; run `p6 api bds` first"),
        )?;
        Ok::<_, String>(false)
    })?;
    super::write(&dir.join("compare.txt"), &report)?;
    out.write_all(&report).map_err(|e| e.to_string())?;
    Ok(played && same)
}

/// `p6 api compare A B`: compares every API scenario's captures in the two folders; a missing
/// capture is a difference.
pub(crate) fn compare(
    root: &Path,
    a: &Path,
    b: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    compare_scenarios(root, a, b, &SCENARIOS, out)
}

/// [`compare()`] for the captures of `scenarios` only. A scenario without a capture in either
/// folder is a difference.
fn compare_scenarios(
    root: &Path,
    a: &Path,
    b: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let deferred = super::load_deferred(root)?;
    let fields = compare::parse_fields(
        &super::read_text(&root.join(compare::FIELDS_PATH))?,
        &deferred,
    )?;
    let intended = parse_intended(&super::read_text(&root.join(INTENDED))?)
        .map_err(|e| format!("{INTENDED}: {e}"))?;
    let mut same = true;
    let with_companions = scenarios
        .iter()
        .flat_map(|s| std::iter::once(*s).chain(companion(s)));
    for scenario in with_companions {
        let scenario = &scenario;
        let capture = |dir: &Path| dir.join(format!("{scenario}.jsonl"));
        if let Some(dir) = [a, b].into_iter().find(|d| !super::is_file(&capture(d))) {
            super::say(
                out,
                &format!(
                    "DIFFERENT {scenario}: no capture in {}; run `p6 api bds {scenario}`",
                    dir.display()
                ),
            )?;
            same = false;
            continue;
        }
        let read = |dir: &Path| {
            steps(&super::read_text(&capture(dir))?)
                .map_err(|e| format!("{}: {e}", capture(dir).display()))
        };
        let checks_only = fields.only(&[]);
        let listed = if CHECKS_ONLY.contains(scenario) {
            &checks_only
        } else {
            &fields
        };
        let (differences, notes) = compare_steps(&read(a)?, &read(b)?, listed);
        for note in &notes {
            super::say(out, &format!("note {scenario}: {note}"))?;
        }
        let facts = FACTS_ONLY.contains(scenario);
        let mut differ = false;
        for d in &differences {
            let kind = match intended.reason(scenario, d) {
                Some(reason) => {
                    super::say(out, &format!("intended {scenario}: {d}\n  why: {reason}"))?;
                    continue;
                }
                None if facts => "note",
                None => {
                    differ = true;
                    "DIFFERENT"
                }
            };
            super::say(out, &format!("{kind} {scenario}: {d}"))?;
        }
        for unseen in intended.unseen(scenario, &differences) {
            super::say(
                out,
                &format!("note {scenario}: intended difference not found: {unseen}"),
            )?;
        }
        super::say(
            out,
            &format!(
                "{} api {scenario}",
                if differ { "DIFFERENT" } else { "same" }
            ),
        )?;
        same &= !differ;
    }
    super::say(out, if same { "SAME" } else { "DIFFERENT" })?;
    Ok(same)
}

/// The intended differences (`INTENDED`): per scenario, the exact difference texts with their
/// reasons.
#[derive(Debug, Default, PartialEq)]
struct Intended(Vec<(String, String, String)>);

impl Intended {
    /// The reason `difference` of `scenario` is intended, if it is listed.
    fn reason(&self, scenario: &str, difference: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(s, d, _)| s == scenario && d == difference)
            .map(|(_, _, reason)| reason.as_str())
    }

    /// The listed differences of `scenario` that are not among `found`.
    fn unseen<'a>(
        &'a self,
        scenario: &'a str,
        found: &'a [String],
    ) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(s, d, _)| s == scenario && !found.contains(d))
            .map(|(_, d, _)| d.as_str())
    }
}

/// Parses the intended-difference file: `#` comments, blank lines, and lines
/// `<scenario> | <difference> | <reason>`, the difference exactly as the comparison prints it
/// after `DIFFERENT <scenario>: ` (it may hold ` | `; the reason may not). A scenario must be an API
/// scenario, the reason must not be empty, and a difference is listed once.
fn parse_intended(text: &str) -> Result<Intended, String> {
    let mut list = Intended::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |e: &str| format!("line {}: {e}", n + 1);
        let (Some((scenario, rest)), Some((_, reason))) =
            (line.split_once(" | "), line.rsplit_once(" | "))
        else {
            return Err(at("want `<scenario> | <difference> | <reason>`"));
        };
        let Some((difference, _)) = rest.rsplit_once(" | ") else {
            return Err(at("want `<scenario> | <difference> | <reason>`"));
        };
        // `p6 import` reads its own lines (`super::import`).
        if scenario == super::import::SCENARIO {
            continue;
        }
        if !SCENARIOS.contains(&scenario) && !COMPANIONS.iter().any(|(_, c)| *c == scenario) {
            return Err(at(&format!("unknown API scenario `{scenario}`")));
        }
        if difference.is_empty() || reason.trim().is_empty() {
            return Err(at("the difference and its reason must not be empty"));
        }
        if list.reason(scenario, difference).is_some() {
            return Err(at("the difference is listed twice"));
        }
        list.0.push((
            scenario.to_owned(),
            difference.to_owned(),
            reason.trim().to_owned(),
        ));
    }
    Ok(list)
}

/// One step of an API capture: the bot's step line and what came until the next one.
#[derive(Debug, Default, PartialEq)]
struct Step {
    /// The step's name and its command line, argument or form kind.
    label: String,
    /// A step that only prepares the world with console commands.
    setup: bool,
    /// The packets received during the step.
    packets: Vec<Received>,
    /// The bot's `output`, `check`, `form` and `available` lines of the step.
    records: Vec<Value>,
}

/// The steps of a capture; the first holds everything before the bot's first step.
fn steps(capture: &str) -> Result<Vec<Step>, String> {
    let mut steps = vec![Step {
        label: "(join)".to_owned(),
        ..Step::default()
    }];
    for (n, line) in capture.lines().enumerate() {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        let text = |key: &str| field(&v, key).as_str().map(str::to_owned);
        let Some(last) = steps.last_mut() else {
            continue;
        };
        match field(&v, "type").as_str() {
            Some("step") => {
                let mut label = text("name").unwrap_or_default();
                if let Some(arg) = text("line").or_else(|| text("arg")) {
                    label = format!("{label} {arg}");
                }
                steps.push(Step {
                    label,
                    setup: field(&v, "setup").as_bool() == Some(true),
                    ..Step::default()
                });
            }
            Some("packet") => last.packets.push(Received {
                name: text("name").unwrap_or_default(),
                fields: field(&v, "fields").clone(),
            }),
            Some("output" | "check" | "form" | "available") => last.records.push(v),
            _ => {}
        }
    }
    Ok(steps)
}

/// Writes `<dir>/<scenario>.txt`, the report of the scenario's capture.
fn write_report(dir: &Path, scenario: &str) -> Result<(), String> {
    let capture = dir.join(format!("{scenario}.jsonl"));
    let steps =
        steps(&super::read_text(&capture)?).map_err(|e| format!("{}: {e}", capture.display()))?;
    super::write(
        &dir.join(format!("{scenario}.txt")),
        report(&steps).as_bytes(),
    )
}

/// The report of a capture's steps (the join step left out).
fn report(steps: &[Step]) -> String {
    let mut text = Vec::new();
    for step in steps.iter().skip(1) {
        text.push(format!("step {}", step.label));
        for r in &step.records {
            let kind = field(r, "type").as_str().unwrap_or_default();
            let line = match kind {
                "available" => format!(
                    "available ({}): {} commands",
                    field(r, "when").as_str().unwrap_or_default(),
                    field(r, "commands").as_array().map_or(0, Vec::len)
                ),
                "output" => format!(
                    "output: uuid_echoed {} state {} packets {}",
                    field(r, "uuid_echoed"),
                    field(r, "state"),
                    field(r, "packets")
                ),
                "check" => format!(
                    "check {}: {}",
                    field(r, "name").as_str().unwrap_or_default(),
                    field(r, "value")
                ),
                _ => format!(
                    "form {}: {} answered {}",
                    field(r, "kind").as_str().unwrap_or_default(),
                    field(r, "data"),
                    field(r, "answer")
                ),
            };
            text.push(format!("  {}", cut(&line)));
        }
        let mut others: Vec<(&str, usize)> = Vec::new();
        for p in &step.packets {
            if REPORTED.contains(&p.name.as_str()) {
                text.push(format!("  #{} {}", p.name, cut(&p.fields.to_string())));
            } else {
                match others.iter_mut().find(|(name, _)| *name == p.name) {
                    Some((_, count)) => *count += 1,
                    None => others.push((&p.name, 1)),
                }
            }
        }
        if !others.is_empty() {
            let list: Vec<String> = others
                .iter()
                .map(|(name, count)| format!("{name} x{count}"))
                .collect();
            text.push(format!("  other: {}", list.join(", ")));
        }
    }
    let mut text = text.join("\n");
    text.push('\n');
    text
}

/// `text` cut to [`REPORT_WIDTH`] characters.
fn cut(text: &str) -> String {
    match text.char_indices().nth(REPORT_WIDTH) {
        Some((end, _)) => format!("{}...", text.get(..end).unwrap_or_default()),
        None => text.to_owned(),
    }
}

/// Compares the steps of two captures (A BDS, B Anchor): the differences, and notes.
fn compare_steps(a: &[Step], b: &[Step], fields: &compare::Fields) -> (Vec<String>, Vec<String>) {
    let mut differences = Vec::new();
    let mut notes = Vec::new();
    let labels = |steps: &[Step]| steps.iter().map(|s| s.label.clone()).collect::<Vec<_>>();
    if labels(a) != labels(b) {
        differences.push(format!(
            "the steps differ:\n  A: {:?}\n  B: {:?}",
            labels(a),
            labels(b)
        ));
        return (differences, notes);
    }
    let setup_fields = fields.only(SETUP_COMPARED);
    for (x, y) in a.iter().zip(b).skip(1) {
        let mut step = Vec::new();
        let listed = if x.setup { &setup_fields } else { fields };
        step.extend(compare::compare_listed(&x.packets, &y.packets, listed));
        compare_records(x, y, &mut step, &mut notes);
        differences.extend(step.into_iter().map(|d| format!("{}: {d}", x.label)));
    }
    // BDS lists every command at spawn, Anchor the operator's after `op`: their last lists.
    if let (Some(x), Some(y)) = (last_available(a), last_available(b)) {
        let when = |r: &Value| field(r, "when").as_str().unwrap_or_default().to_owned();
        for note in compare_available(field(x, "commands"), field(y, "commands")) {
            notes.push(format!(
                "AvailableCommands (A {}, B {}): {note}",
                when(x),
                when(y)
            ));
        }
    }
    (differences, notes)
}

/// Compares the bot's lines of two paired steps.
fn compare_records(a: &Step, b: &Step, differences: &mut Vec<String>, notes: &mut Vec<String>) {
    let of = |s: &Step, kind: &str| -> Vec<Value> {
        s.records
            .iter()
            .filter(|r| field(r, "type").as_str() == Some(kind))
            .cloned()
            .collect()
    };
    let (outputs_a, outputs_b) = (of(a, "output"), of(b, "output"));
    for (x, y) in outputs_a.iter().zip(&outputs_b) {
        for key in ["uuid_echoed", "state"] {
            if field(x, key) != field(y, key) {
                differences.push(format!(
                    "output {key}: A {} != B {}",
                    field(x, key),
                    field(y, key)
                ));
            }
        }
        if field(x, "output").is_null() != field(y, "output").is_null() {
            differences.push(format!(
                "CommandOutput: A {} B {}",
                presence(field(x, "output")),
                presence(field(y, "output"))
            ));
        }
    }
    let (checks_a, checks_b) = (of(a, "check"), of(b, "check"));
    let names = |checks: &[Value]| -> Vec<String> {
        checks
            .iter()
            .map(|c| field(c, "name").as_str().unwrap_or_default().to_owned())
            .collect()
    };
    if names(&checks_a) != names(&checks_b) {
        differences.push(format!(
            "checks: A {:?} != B {:?}",
            names(&checks_a),
            names(&checks_b)
        ));
    }
    for (x, y) in checks_a.iter().zip(&checks_b) {
        // The disconnect check repeats the Disconnect packet, which the fixed fields compare.
        let name = field(x, "name").as_str().unwrap_or_default();
        if name == "disconnect" || field(x, "value") == field(y, "value") {
            continue;
        }
        let text = format!(
            "check {name}: A {} != B {}",
            field(x, "value"),
            field(y, "value")
        );
        if NOTE_CHECKS.contains(&name) {
            notes.push(text);
        } else {
            differences.push(text);
        }
    }
    let (forms_a, forms_b) = (of(a, "form"), of(b, "form"));
    if forms_a.len() != forms_b.len() {
        differences.push(format!("forms: A {} != B {}", forms_a.len(), forms_b.len()));
    }
    for (x, y) in forms_a.iter().zip(&forms_b) {
        let (fx, fy) = (numbers(field(x, "data")), numbers(field(y, "data")));
        if fx != fy {
            differences.push(format!("form JSON: A {fx} != B {fy}"));
        }
    }
}

/// The last command list of a capture: the operator's when the server resent it after `op`.
fn last_available(steps: &[Step]) -> Option<&Value> {
    steps
        .iter()
        .flat_map(|s| &s.records)
        .rfind(|r| field(r, "type").as_str() == Some("available"))
}

fn presence(value: &Value) -> &'static str {
    if value.is_null() { "none" } else { "sent" }
}

/// `value` with every number as a float, so `3` and `3.0` compare equal.
fn numbers(value: &Value) -> Value {
    match value {
        Value::Number(n) => n
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| value.clone(), Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(numbers).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), numbers(v))).collect())
        }
        _ => value.clone(),
    }
}

/// Notes on two command lists (the bot's `available` summaries): the names only one has, and for
/// the commands both have, differing aliases, permission levels, flags and overloads.
fn compare_available(a: &Value, b: &Value) -> Vec<String> {
    let by_name = |list: &Value| -> std::collections::BTreeMap<String, Value> {
        list.as_array()
            .map(|l| {
                l.iter()
                    .map(|c| {
                        (
                            field(c, "name").as_str().unwrap_or_default().to_owned(),
                            c.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let (a, b) = (by_name(a), by_name(b));
    let only = |x: &std::collections::BTreeMap<String, Value>,
                y: &std::collections::BTreeMap<String, Value>| {
        x.keys()
            .filter(|k| !y.contains_key(*k))
            .cloned()
            .collect::<Vec<_>>()
    };
    let both: Vec<&String> = a.keys().filter(|k| b.contains_key(*k)).collect();
    let mut notes = vec![
        format!("A {} commands, B {}, both {}", a.len(), b.len(), both.len()),
        format!("only A: {}", only(&a, &b).join(" ")),
        format!("only B: {}", only(&b, &a).join(" ")),
    ];
    for name in both {
        let (Some(x), Some(y)) = (a.get(name), b.get(name)) else {
            continue;
        };
        for key in ["permission_level", "flags"] {
            if field(x, key) != field(y, key) {
                notes.push(format!(
                    "{name} {key}: A {} B {}",
                    field(x, key),
                    field(y, key)
                ));
            }
        }
        let aliases = |c: &Value| -> Vec<String> {
            let mut list: Vec<String> = field(c, "aliases")
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            list.sort();
            list
        };
        if aliases(x) != aliases(y) {
            notes.push(format!(
                "{name} aliases: A {:?} B {:?}",
                aliases(x),
                aliases(y)
            ));
        }
        let (ox, oy) = (overloads(x), overloads(y));
        if ox != oy {
            notes.push(format!("{name} overloads:\n    A {ox:?}\n    B {oy:?}"));
        }
    }
    notes
}

/// A command's overloads as `type` or `type?` (optional) per parameter, parameter names left
/// out (they are labels).
fn overloads(command: &Value) -> Vec<String> {
    field(command, "overloads")
        .as_array()
        .map(|list| {
            list.iter()
                .map(|o| {
                    let params: Vec<String> = field(o, "params")
                        .as_array()
                        .map(|ps| {
                            ps.iter()
                                .map(|p| {
                                    let t = field(p, "type").as_str().unwrap_or_default();
                                    let optional = field(p, "optional").as_bool() == Some(true);
                                    format!("{t}{}", if optional { "?" } else { "" })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    params.join(" ")
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = concat!(
        r#"{"type":"packet","name":"StartGame","fields":{}}"#,
        "\n",
        r#"{"type":"step","name":"op","setup":true}"#,
        "\n",
        r#"{"type":"packet","name":"AvailableCommands","fields":{}}"#,
        "\n",
        r#"{"type":"step","name":"command","line":"time set 1000"}"#,
        "\n",
        r#"{"type":"packet","name":"SetTime","fields":{"Time":1000}}"#,
        "\n",
        r#"{"type":"output","line":"time set 1000","uuid_echoed":true,"state":{"stone":0},"output":{}}"#,
        "\n",
    );

    #[test]
    fn captures_split_into_labelled_steps() {
        let steps = steps(CAPTURE).expect("steps");
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["(join)", "op", "command time set 1000"]);
        assert!(steps[1].setup);
        assert_eq!(steps[2].packets[0].name, "SetTime");
        assert_eq!(steps[2].records.len(), 1);
        assert!(report(&steps).contains(r#"#SetTime {"Time":1000}"#));
    }

    #[test]
    fn steps_differ_by_fixed_fields_and_bot_state() {
        let fields =
            compare::parse_fields("SetTime Time\n", &compare::Deferred::default()).expect("fields");
        let a = steps(CAPTURE).expect("a");
        assert!(compare_steps(&a, &a, &fields).0.is_empty());
        let b = steps(
            &CAPTURE
                .replace(r#""Time":1000"#, r#""Time":0"#)
                .replace(r#""stone":0"#, r#""stone":3"#),
        )
        .expect("b");
        let (differences, _) = compare_steps(&a, &b, &fields);
        assert_eq!(differences.len(), 2, "{differences:?}");
    }

    #[test]
    fn form_json_numbers_compare_as_numbers() {
        assert_eq!(
            numbers(&serde_json::json!({"max": 10, "step": 1.0})),
            numbers(&serde_json::json!({"max": 10.0, "step": 1}))
        );
    }

    #[test]
    fn command_lists_note_only_shared_shape_differences() {
        let a = serde_json::json!([
            {"name": "time", "aliases": [], "permission_level": 1, "flags": 0,
             "overloads": [{"params": [{"type": "enum:TimeSet", "optional": false}]}]},
            {"name": "fill", "aliases": [], "overloads": []},
        ]);
        let b = serde_json::json!([
            {"name": "time", "aliases": [], "permission_level": 0, "flags": 0,
             "overloads": [{"params": [{"type": "string", "optional": true}]}]},
        ]);
        let notes = compare_available(&a, &b);
        assert!(notes.iter().any(|n| n == "only A: fill"), "{notes:?}");
        assert!(
            notes.iter().any(|n| n.starts_with("time overloads")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.starts_with("time permission_level")),
            "{notes:?}"
        );
    }

    #[test]
    fn intended_differences_pass_only_when_listed_exactly() {
        let intended = parse_intended(concat!(
            "# comment\n",
            "\n",
            "commands | command say hi: Text: A 1 != B 2 | PMMP's say\n",
            "kick | x | y | z | the middle keeps its bars\n",
        ))
        .expect("intended");
        assert_eq!(
            intended.reason("commands", "command say hi: Text: A 1 != B 2"),
            Some("PMMP's say")
        );
        assert_eq!(
            intended.reason("kick", "x | y | z"),
            Some("the middle keeps its bars")
        );
        // Another scenario, a changed value or a part of the text is not listed.
        assert_eq!(
            intended.reason("forms", "command say hi: Text: A 1 != B 2"),
            None
        );
        assert_eq!(
            intended.reason("commands", "command say hi: Text: A 1 != B 3"),
            None
        );
        assert_eq!(intended.reason("commands", "command say hi"), None);
        let found = vec!["other".to_owned()];
        assert_eq!(
            intended.unseen("commands", &found).collect::<Vec<_>>(),
            ["command say hi: Text: A 1 != B 2"]
        );
        for bad in [
            "commands | no reason",
            "trade | a | b",
            "commands | a |  ",
            "commands | a | b\ncommands | a | c",
        ] {
            assert!(parse_intended(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_intended_difference_file_parses() {
        let text = include_str!("../../data/p6/intended.txt");
        let intended = parse_intended(text).expect("intended");
        assert!(!intended.0.is_empty());
    }

    #[test]
    fn unknown_api_scenarios_are_refused() {
        assert_eq!(scenarios(&[]).expect("all"), SCENARIOS.to_vec());
        assert!(scenarios(&["trade"]).is_err());
    }

    #[test]
    fn a_missing_capture_of_a_requested_scenario_fails() {
        let dir = std::env::temp_dir().join(format!("anchor-p6-api-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        for (d, scenario) in [(&a, "commands"), (&b, "commands"), (&a, "kick")] {
            super::super::create_dir(d).expect("dir");
            super::super::write(&d.join(format!("{scenario}.jsonl")), b"").expect("capture");
        }
        let mut out = Vec::new();
        let same = compare_scenarios(&crate::root(), &a, &b, &["commands", "kick"], &mut out);
        super::super::remove_dir(&dir).expect("cleanup");
        assert_eq!(same, Ok(false));
        let report = String::from_utf8_lossy(&out);
        assert!(report.contains("DIFFERENT kick: no capture in"), "{report}");
    }
}
