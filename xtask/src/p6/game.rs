//! The game scenarios of `cargo xtask p6`: `craft`, `furnace`, `enchant`,
//! `crops` and `trade`. The bot plays each one like the official client
//! (`xtask/go/bot/game.go`) and writes the console commands it needs into its capture as `command`
//! records; the harness runs
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

pub(super) mod console;
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
const BOT_TIMEOUT: Duration = Duration::from_secs(150);
/// How long the harness waits for a bot past its own time limit before killing it.
const BOT_GRACE: Duration = Duration::from_secs(30);

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
    let exe = super::bds_exe(root)?;
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
    let blocks = super::block_states(root)?;
    let dir = root.join(super::OUT_DIR).join("anchor-game");
    let (mut console, task) = console::console();
    let with_console = |b: anchor::ServerBuilder| b.every(console::EVERY_TICK, task);
    let played = super::anchor_session(
        &dir,
        "zlib",
        |_| {},
        with_console,
        out,
        |addr, _, out| {
            let mut passed = run_spawn(bot, addr, &dir, out)?;
            for line in SESSION_SETUP {
                console.send(line)?;
            }
            passed &= console.settle(out)?;
            for scenario in scenarios {
                passed &= run_scenario(
                    bot,
                    addr,
                    &dir,
                    scenario,
                    None,
                    |line| console.send(line),
                    out,
                )?;
                passed &= console.settle(out)?;
                write_report(&dir, scenario, &blocks)?;
            }
            Ok(passed)
        },
    )?;
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
    let blocks = super::block_states(root)?;
    let dir = root.join(super::OUT_DIR).join("bds-game");
    super::bds_session(exe, &dir, "raknet", "zlib", out, |addr, server, out| {
        let mut passed = run_spawn(bot, addr, &dir, out)?;
        for line in SESSION_SETUP {
            server.command(line)?;
        }
        for scenario in scenarios {
            passed &= run_scenario(
                bot,
                addr,
                &dir,
                scenario,
                None,
                |line| server.command(line),
                out,
            )?;
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

/// `p6 game compare A B`: compares the traces of every game scenario's captures in the two folders
/// and prints the differences; true when there are none and no capture is missing.
pub(crate) fn compare(
    root: &Path,
    a: &Path,
    b: &Path,
    out: &mut impl Write,
) -> Result<bool, String> {
    compare_scenarios(root, a, b, &SCENARIOS, out)
}

/// [`compare`] for the captures of `scenarios` only. A scenario without a capture in either
/// folder is a difference.
fn compare_scenarios(
    root: &Path,
    a: &Path,
    b: &Path,
    scenarios: &[&str],
    out: &mut impl Write,
) -> Result<bool, String> {
    let blocks = super::block_states(root)?;
    let mut same = true;
    for scenario in scenarios {
        let capture = |dir: &Path| dir.join(format!("{scenario}.jsonl"));
        if let Some(dir) = [a, b].into_iter().find(|d| !super::is_file(&capture(d))) {
            super::say(
                out,
                &format!(
                    "DIFFERENT {scenario}: no capture in {}; run `p6 game bds {scenario}`",
                    dir.display()
                ),
            )?;
            same = false;
            continue;
        }
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
/// `console` as it appears, and returns whether the bot passed and its failure text. `script` is
/// the action file of the `script` scenario.
pub(crate) fn run_scenario(
    bot: &Path,
    addr: &str,
    dir: &Path,
    scenario: &str,
    script: Option<&Path>,
    console: impl FnMut(&str) -> Result<(), String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    run_scenarios(bot, addr, dir, scenario, script, None, console, out)
}

/// One bot of [`run_scenarios`]: its scenario, capture, process and the commands relayed.
struct Running {
    scenario: String,
    capture: std::path::PathBuf,
    child: std::process::Child,
    relayed: usize,
}

impl Running {
    /// Starts the bot of `scenario` into `<dir>/<scenario>.jsonl`.
    fn start(
        bot: &Path,
        addr: &str,
        dir: &Path,
        scenario: &str,
        script: Option<&Path>,
    ) -> Result<Self, String> {
        let capture = dir.join(format!("{scenario}.jsonl"));
        super::remove_file(&capture)?;
        let name = player(scenario);
        let timeout = format!("{}s", BOT_TIMEOUT.as_secs());
        let mut command = Command::new(bot);
        if let Some(script) = script {
            command.arg("-script").arg(script);
        }
        let child = command
            .args(["-addr", addr, "-transport", "raknet", "-scenario", scenario])
            .args(["-name", &name, "-timeout", &timeout, "-out"])
            .arg(&capture)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run the bot: {e}"))?;
        Ok(Self {
            scenario: scenario.to_owned(),
            capture,
            child,
            relayed: 0,
        })
    }

    /// Passes the bot's new command records to `console`; true once the bot has exited.
    fn relay(
        &mut self,
        console: &mut impl FnMut(&str) -> Result<(), String>,
    ) -> Result<bool, String> {
        let exited = self
            .child
            .try_wait()
            .map_err(|e| format!("bot: {e}"))?
            .is_some();
        // The capture may not exist yet; a missing file has no commands.
        let text = super::read_text(&self.capture).unwrap_or_default();
        for line in commands(&text).iter().skip(self.relayed) {
            console(line)?;
            self.relayed += 1;
        }
        Ok(exited)
    }

    /// Whether the bot has started its first step (it has joined and teleported).
    fn stepped(&self) -> bool {
        super::read_text(&self.capture).is_ok_and(|t| t.contains(r#""type":"step""#))
    }

    /// Waits for the bot, killing it first when `kill`, and reports its result.
    fn finish(mut self, kill: bool, out: &mut impl Write) -> Result<bool, String> {
        if kill {
            self.child
                .kill()
                .map_err(|e| format!("kill the bot: {e}"))?;
        }
        let output = self
            .child
            .wait_with_output()
            .map_err(|e| format!("wait for the bot: {e}"))?;
        let passed = output.status.success();
        let failure = if passed {
            String::new()
        } else {
            format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
        };
        super::say(
            out,
            &format!(
                "{} game {} ({} commands){failure}",
                if passed { "ok  " } else { "FAIL" },
                self.scenario,
                self.relayed
            ),
        )?;
        Ok(passed)
    }
}

/// [`run_scenario`], with the bot of `companion` started into `<dir>/<companion>.jsonl` once the
/// scenario's bot has begun its first step (a second player the scenario watches); the commands
/// of both are relayed, and both must pass.
#[expect(
    clippy::too_many_arguments,
    reason = "consistency: run_scenario's arguments and the companion"
)]
pub(crate) fn run_scenarios(
    bot: &Path,
    addr: &str,
    dir: &Path,
    scenario: &str,
    script: Option<&Path>,
    companion: Option<&str>,
    mut console: impl FnMut(&str) -> Result<(), String>,
    out: &mut impl Write,
) -> Result<bool, String> {
    let mut main = Running::start(bot, addr, dir, scenario, script)?;
    let mut second: Option<Running> = None;
    let deadline = super::now() + BOT_TIMEOUT + BOT_GRACE;
    let relay = loop {
        if let (Some(c), None) = (companion, &second)
            && main.stepped()
        {
            second = Some(Running::start(bot, addr, dir, c, None)?);
        }
        let exited = match main.relay(&mut console) {
            Ok(e) => e,
            Err(e) => break Err(e),
        };
        let companion_exited = match second.as_mut().map(|c| c.relay(&mut console)) {
            Some(Ok(e)) => e,
            Some(Err(e)) => break Err(e),
            // A scenario bot that ended before it stepped never starts its companion.
            None => companion.is_none() || exited,
        };
        if exited && companion_exited {
            break Ok(());
        }
        if super::now() > deadline {
            break Err(format!("the {scenario} bot did not end in time"));
        }
        super::pause();
    };
    let kill = relay.is_err();
    let mut passed = main.finish(kill, out)?;
    if let Some(c) = second {
        passed &= c.finish(kill, out)?;
    }
    relay?;
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

    #[test]
    fn a_missing_capture_of_a_requested_scenario_fails() {
        let dir = std::env::temp_dir().join(format!("anchor-p6-game-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        let items = br#"{"type":"packet","name":"ItemRegistry","fields":{"Items":[]}}"#;
        for (d, scenario) in [(&a, "craft"), (&b, "craft"), (&a, "furnace")] {
            super::super::create_dir(d).expect("dir");
            super::super::write(&d.join(format!("{scenario}.jsonl")), b"").expect("capture");
            super::super::write(&d.join(format!("{scenario}.items.jsonl")), items).expect("items");
        }
        let mut out = Vec::new();
        let same = compare_scenarios(&crate::root(), &a, &b, &["craft", "furnace"], &mut out);
        super::super::remove_dir(&dir).expect("cleanup");
        assert_eq!(same, Ok(false));
        let report = String::from_utf8_lossy(&out);
        assert!(
            report.contains("DIFFERENT furnace: no capture in"),
            "{report}"
        );
    }
}
