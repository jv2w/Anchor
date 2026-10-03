//! The game scenarios of `cargo xtask p6` (PLAN.md 10.7 P6): `craft`, `furnace`, `enchant`,
//! `crops` and `trade`. The bot plays each one like the official client (`xtask/go/bot/game.go`)
//! and writes the console commands it needs into its capture as `command` records; the harness runs
//! each new command on the server's console while the bot runs (`run_scenario`). `p6 game bds` runs
//! them against a fresh BDS world into `target/p6/bds-game/`: first the `spawn` scenario with
//! untouched game rules (the spawn window of this world), then `SESSION_SETUP`, then each game
//! scenario with its own player name. Each capture gets a report (`<scenario>.txt`, see `trace`),
//! and `p6 game compare A B` compares the fixed parts of the traces of two capture folders.
//! `p6 game anchor` runs the same session against Anchor, started in this process with the P6
//! settings (RakNet, zlib, a fresh flat world, as `p6 anchor`), into `target/p6/anchor-game/`: its
//! `console` carries out the commands through Anchor's public task API (`console`), and the
//! session ends with `p6 game compare target/p6/bds-game target/p6/anchor-game` for the
//! scenarios it ran, also written to `anchor-game/compare.txt`.

mod console;
mod trace;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The game scenarios, in run order.
pub(crate) const SCENARIOS: [&str; 5] = ["craft", "furnace", "enchant", "crops", "trade"];
/// Console commands run once before the game scenarios: no mob spawning, weather, day cycle or
/// random ticks (grass under a block turning to dirt), so no unrelated entity, time or block traffic
/// lands between the steps; then every mob is removed.
pub(crate) const SESSION_SETUP: &[&str] = &[
    "gamerule domobspawning false",
    "gamerule randomtickspeed 0",
    "gamerule dodaylightcycle false",
    "gamerule doweathercycle false",
    "kill @e[type=!player]",
];
/// The bot's time limit for one game scenario (the furnace smelts for 20 seconds).
const BOT_TIMEOUT: &str = "150s";
/// How long the harness waits for a bot past its own time limit before killing it.
const BOT_GRACE: Duration = Duration::from_secs(180);

/// The player name of a scenario's bot: one new player each, so no inventory carries over.
pub(crate) fn player(scenario: &str) -> String {
    let mut name = String::from("P6");
    let mut chars = scenario.chars();
    if let Some(first) = chars.next() {
        name.extend(first.to_uppercase());
        name.push_str(chars.as_str());
    }
    name
}

/// Picks the scenarios named in `args` (all when empty).
pub(crate) fn scenarios<'a>(args: &[&'a str]) -> Result<Vec<&'a str>, String> {
    if args.is_empty() {
        return Ok(SCENARIOS.to_vec());
    }
    args.iter()
        .map(|a| {
            SCENARIOS
                .iter()
                .find(|s| *s == a)
                .map(|_| *a)
                .ok_or_else(|| format!("unknown game scenario `{a}` (want {SCENARIOS:?})"))
        })
        .collect()
}

/// `p6 game bds [scenario]...`: the game session of `session` alone.
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

/// `p6 game anchor [scenario]...`: [`anchor_session`] alone.
pub(crate) fn anchor(root: &Path, args: &[&str], out: &mut impl Write) -> Result<bool, String> {
    let scenarios = scenarios(args)?;
    let bot = super::build_bot(root)?;
    let passed = anchor_session(root, &bot, &scenarios, out)?;
    super::say(out, if passed { "PASS" } else { "FAIL" })?;
    Ok(passed)
}

/// The game session against Anchor into `target/p6/anchor-game/`, then its comparison with the
/// BDS session's captures (`p6 game bds` makes them). It passes when every bot and every console
/// command passed and the traces are the same.
pub(crate) fn anchor_session(
    root: &Path,
    bot: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let blocks = super::block_states(&super::read_text(&root.join(super::REFERENCE_BLOCKS))?)?;
    let dir = root.join(super::OUT_DIR).join("anchor-game");
    let (mut console, task) = console::console();
    let with_console = |b: anchor::ServerBuilder| b.every(console::EVERY_TICK, task);
    let played = super::anchor_session(&dir, "zlib", None, with_console, out, |addr, out| {
        let mut passed = run_spawn(bot, addr, &dir, out)?;
        for line in SESSION_SETUP {
            console.send(line)?;
        }
        passed &= console.settle(out)?;
        for scenario in scenarios {
            passed &= run_scenario(bot, addr, &dir, scenario, |line| console.send(line), out)?;
            passed &= console.settle(out)?;
            write_report(&dir, scenario, &blocks)?;
        }
        Ok(passed)
    })?;
    let bds = root.join(super::OUT_DIR).join("bds-game");
    let mut report = Vec::new();
    super::say(&mut report, "compare bds-game anchor-game:")?;
    // Missing BDS captures fail the comparison, not the run.
    let same = compare_scenarios(root, &bds, &dir, scenarios, &mut report).or_else(|e| {
        super::say(
            &mut report,
            &format!("DIFFERENT: {e}; run `p6 game bds` first"),
        )?;
        Ok::<_, String>(false)
    })?;
    super::write(&dir.join("compare.txt"), &report)?;
    out.write_all(&report).map_err(|e| e.to_string())?;
    Ok(played && same)
}

/// One BDS session (RakNet, zlib, a fresh world) that records the spawn window, runs
/// `SESSION_SETUP` and the game scenarios into `target/p6/bds-game/`, each with its report.
pub(crate) fn session(
    root: &Path,
    exe: &Path,
    bot: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let blocks = super::block_states(&super::read_text(&root.join(super::REFERENCE_BLOCKS))?)?;
    let dir = root.join(super::OUT_DIR).join("bds-game");
    super::bds_session(exe, &dir, "raknet", "zlib", out, |addr, server, out| {
        let mut passed = run_spawn(bot, addr, &dir, out)?;
        for line in SESSION_SETUP {
            server.command(line)?;
        }
        for scenario in scenarios {
            passed &= run_scenario(bot, addr, &dir, scenario, |line| server.command(line), out)?;
            write_report(&dir, scenario, &blocks)?;
        }
        Ok(passed)
    })
}

/// Writes `<dir>/<scenario>.txt`, the trace of the scenario's capture, and returns the trace.
fn write_report(
    dir: &Path,
    scenario: &str,
    blocks: &BTreeMap<u32, String>,
) -> Result<Vec<trace::Step>, String> {
    let steps = read_trace(dir, scenario, blocks)?;
    super::write(
        &dir.join(format!("{scenario}.txt")),
        trace::report(&steps).as_bytes(),
    )?;
    Ok(steps)
}

/// The trace of `<dir>/<scenario>.jsonl`, with the item names of its items file.
fn read_trace(
    dir: &Path,
    scenario: &str,
    blocks: &BTreeMap<u32, String>,
) -> Result<Vec<trace::Step>, String> {
    let capture = dir.join(format!("{scenario}.jsonl"));
    let items = trace::item_names(&super::read_text(&super::items_path(&capture))?)
        .map_err(|e| format!("{}: {e}", capture.display()))?;
    trace::trace(&super::read_text(&capture)?, &items, blocks)
        .map_err(|e| format!("{}: {e}", capture.display()))
}

/// `p6 game compare A B`: compares the traces of the game scenario captures that both folders
/// hold and prints the differences; true when there are none.
pub(crate) fn compare(
    root: &Path,
    a: &Path,
    b: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    compare_scenarios(root, a, b, &SCENARIOS, out)
}

/// [`compare`] for the captures of `scenarios` only.
fn compare_scenarios(
    root: &Path,
    a: &Path,
    b: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let blocks = super::block_states(&super::read_text(&root.join(super::REFERENCE_BLOCKS))?)?;
    let mut same = true;
    let mut compared = 0;
    for scenario in scenarios {
        let capture = |dir: &Path| dir.join(format!("{scenario}.jsonl"));
        if !super::is_file(&capture(a)) || !super::is_file(&capture(b)) {
            continue;
        }
        compared += 1;
        let differences = trace::compare(
            &read_trace(a, scenario, &blocks)?,
            &read_trace(b, scenario, &blocks)?,
        );
        for d in &differences {
            super::say(out, &format!("{scenario}: {d}"))?;
        }
        super::say(
            out,
            &format!(
                "{} game {scenario}",
                if differences.is_empty() {
                    "same"
                } else {
                    "DIFFERENT"
                }
            ),
        )?;
        same &= differences.is_empty();
    }
    if compared == 0 {
        return Err("the folders hold no game scenario captures in common".to_owned());
    }
    super::say(out, if same { "SAME" } else { "DIFFERENT" })?;
    Ok(same)
}

/// Records the spawn window of the session's world into `<dir>/spawn.jsonl`.
fn run_spawn(bot: &Path, addr: &str, dir: &Path, out: &mut impl Write) -> Result<bool, String> {
    let args = ["-addr", addr, "-transport", "raknet", "-scenario", "spawn"];
    let outcome = super::run_bot(bot, &args, &dir.join("spawn.jsonl"))?;
    super::say(
        out,
        &format!(
            "{} raknet spawn{}",
            if outcome.passed { "ok  " } else { "FAIL" },
            outcome.failure()
        ),
    )?;
    Ok(outcome.passed)
}

/// Runs one game scenario's bot into `<dir>/<scenario>.jsonl`, passing each command record to
/// `console` as it appears, and returns whether the bot passed and its failure text.
pub(crate) fn run_scenario(
    bot: &Path,
    addr: &str,
    dir: &Path,
    scenario: &str,
    mut console: impl FnMut(&str) -> Result<(), String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let capture = dir.join(format!("{scenario}.jsonl"));
    super::remove_file(&capture)?;
    let name = player(scenario);
    let mut child = Command::new(bot)
        .args(["-addr", addr, "-transport", "raknet", "-scenario", scenario])
        .args(["-name", &name, "-timeout", BOT_TIMEOUT, "-out"])
        .arg(&capture)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the bot: {e}"))?;
    let deadline = super::now() + BOT_GRACE;
    let mut relayed = 0;
    let relay = loop {
        let exited = child.try_wait().map_err(|e| format!("bot: {e}"))?.is_some();
        // The capture may not exist yet; a missing file has no commands.
        let text = super::read_text(&capture).unwrap_or_default();
        let commands = commands(&text);
        let mut failed = None;
        for line in commands.iter().skip(relayed) {
            if let Err(e) = console(line) {
                failed = Some(e);
                break;
            }
            relayed += 1;
        }
        if let Some(e) = failed {
            break Err(e);
        }
        if exited {
            break Ok(());
        }
        if super::now() > deadline {
            break Err(format!("the {scenario} bot did not end in time"));
        }
        super::pause();
    };
    if relay.is_err() {
        child.kill().map_err(|e| format!("kill the bot: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait for the bot: {e}"))?;
    relay?;
    let passed = output.status.success();
    let failure = if passed {
        String::new()
    } else {
        format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
    };
    super::say(
        out,
        &format!(
            "{} game {scenario} ({relayed} commands){failure}",
            if passed { "ok  " } else { "FAIL" }
        ),
    )?;
    Ok(passed)
}

/// The command lines of a capture's complete `command` records, in order.
fn commands(capture: &str) -> Vec<String> {
    let complete = capture
        .rfind('\n')
        .and_then(|end| capture.get(..end))
        .unwrap_or_default();
    complete
        .lines()
        .filter(|line| line.contains(r#""type":"command""#))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v.get("line").and_then(|l| l.as_str()).map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn players_are_named_after_their_scenario() {
        assert_eq!(player("craft"), "P6Craft");
        assert_eq!(player("trade"), "P6Trade");
    }

    #[test]
    fn only_complete_command_lines_are_relayed() {
        let capture = concat!(
            r#"{"type":"command","line":"give P6Craft oak_log 4"}"#,
            "\n",
            r#"{"type":"step","name":"x"}"#,
            "\n",
            r#"{"type":"command","line":"tp P6Craft 1 2 3"}"#,
            "\n",
            r#"{"type":"command","line":"unfinished"#,
        );
        assert_eq!(
            commands(capture),
            ["give P6Craft oak_log 4", "tp P6Craft 1 2 3"]
        );
    }

    #[test]
    fn unknown_scenarios_are_refused() {
        assert_eq!(scenarios(&[]).expect("all"), SCENARIOS.to_vec());
        assert_eq!(scenarios(&["trade"]).expect("one"), ["trade"]);
        assert!(scenarios(&["fishing"]).is_err());
    }
}
