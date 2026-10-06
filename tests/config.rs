//! The settings file through the public API: the default file, every key's defaults, types and
//! ranges, unknown keys, and file handling.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::error::Error as _;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use anchor::config::{
    Difficulty, Error, GameMode, IntegrityCheck, LevelType, Lighting, LogLevel, Threads, Update,
    WorldSettings,
};
use anchor::protocol::compression::Compression;
use anchor::world::{BlockPos, Flat, Kind};
use anchor::{Config, Server};

const MIB: usize = 1024 * 1024;

/// A settings file in its own fresh directory under the test target directory.
struct File {
    path: PathBuf,
}

impl File {
    fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("config")
            .join(test);
        fresh_dir(&dir);
        Self {
            path: dir.join("anchor.toml"),
        }
    }

    /// Writes `text` and loads it over `config`.
    fn load_over(&self, config: Config, text: impl AsRef<[u8]>) -> Result<Config, Error> {
        write(&self.path, text.as_ref());
        config.load(&self.path)
    }

    /// Writes `text` and loads it over the defaults.
    fn load(&self, text: impl AsRef<[u8]>) -> Result<Config, Error> {
        self.load_over(Config::default(), text)
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn fresh_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => panic!("cannot clear {}: {e}", dir.display()),
    }
    std::fs::create_dir_all(dir).expect("create the test directory");
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write the settings file");
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read a text file")
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-tick: test tooling outside the server"
)]
fn exists(path: &Path) -> bool {
    std::fs::exists(path).expect("check a path")
}

/// Returns the error and its sources, joined with ": ".
fn chain(error: &Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Returns the first toml block of `.claude/docs/config.md`: the documented default file.
fn documented_default_file() -> String {
    let doc = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/docs/config.md"));
    let (_, block) = doc
        .split_once("```toml\n")
        .expect(".claude/docs/config.md has a toml block");
    let (block, _) = block.split_once("\n```").expect("the toml block closes");
    format!("{block}\n")
}

/// Returns `(table, key, value)` for every key line of a settings file.
fn key_lines(text: &str) -> Vec<(String, String, String)> {
    let mut table = String::new();
    let mut keys = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('[') {
            table = name.trim_end_matches(']').to_owned();
        } else if let Some((key, value)) = line.split_once(" = ")
            && !line.starts_with('#')
        {
            keys.push((table.clone(), key.to_owned(), value.to_owned()));
        }
    }
    keys
}

/// Returns the blocks of a settings file: each key line, by path (`table.key`), and each table
/// header, by `[table]`, with the comment lines right above it.
fn blocks(text: &str) -> Vec<(String, String)> {
    let mut table = String::new();
    let mut comment = String::new();
    let mut blocks = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with('#') {
            comment.push_str(line);
            continue;
        }
        let block = std::mem::take(&mut comment) + line;
        if let Some(name) = line.trim_end().strip_prefix('[') {
            table = name.trim_end_matches(']').to_owned();
            blocks.push((format!("[{table}]"), block));
        } else if let Some((key, _)) = line.split_once(" = ") {
            blocks.push((format!("{table}.{key}"), block));
        }
    }
    blocks
}

/// Writes `text` and updates it; checks that the file means what `text` meant and that a
/// second update changes nothing. Returns the update and the updated text.
fn write_and_update(file: &File, text: &str) -> (Update, String) {
    let before = file.load(text).expect("the file loads before the update");
    let update = Config::update_file(&file.path).expect("updates");
    let updated = read(&file.path);
    let after = Config::default().load(&file.path);
    assert_eq!(after.expect("the file loads after the update"), before);
    assert!(
        Config::update_file(&file.path)
            .expect("updates again")
            .is_empty()
    );
    assert_eq!(read(&file.path), updated);
    assert!(!exists(&file.path.with_extension("toml.tmp")));
    (update, updated)
}

/// Returns a file that sets only `path` (`table.key`) to `value`.
fn one_key(path: &str, value: &str) -> String {
    let (table, key) = path.split_once('.').expect("a dotted key");
    format!("[{table}]\n{key} = {value}\n")
}

/// A file that sets every key to a value other than its default.
const CUSTOM_FILE: &str = r#"[general]
motd = "Lobby"
server-ip = "127.0.0.1"
server-ipv6 = "::1"
enable-ipv6 = false
server-port = 19140
nethernet-port = 19141
public-ip = "203.0.113.7"
max-players = 100
xbox-auth = false
white-list = true
gamemode = "creative"
force-gamemode = true
difficulty = "hard"
hardcore = true
pvp = false
hunger = false
experience = false
experience-orbs = false
fall-damage = false
drowning = false
armor = false
durability = false
beds = false
daylight-cycle = false
recipe-book = false
vanilla-recipes = false
education-items = false
item-despawn-ticks = 1200
sprint-critical-hits = true
break-time-check = false
movement-check = false
level-name = "lobby"
level-type = "void"
level-seed = "-42"
view-distance = 8
save-player-data = false
backups = false
shutdown-message = "Bye"
resource-packs = ["a.mcpack", "lobby/b.zip"]
force-resources = true
log-level = "debug"

[developer]
compression = "zlib"
compression-threshold = 512
compression-level = 9
max-batch-bytes = 1048576
enable-encryption = false
raknet = false
max-mtu-size = 1200
nethernet-tls-cert = "cert.pem"
nethernet-tls-key = "key.pem"
network-threads = 2
cpu-threads = 3
chunks-per-tick = 8
spawn-radius = 6
tick-radius = 5
random-tick-speed = 0
disable-block-ticking = ["minecraft:grass_block"]
slow-empty-worlds = true
lighting = "on-load"
fixed-light-level = 7
item-pickup-interval = 4
hook-movement-packets = false
hook-player-input = false
save-interval-minutes = 5
integrity-check = "never"
console-input = false

[worlds.lobby]
view-distance = 6
chunks-per-tick = 2
spawn-radius = 3
tick-radius = 2
spawn = [-2147483648, -60, 2147483647]

[worlds.arena]
tick-radius = 1
"#;

/// The config [`CUSTOM_FILE`] describes.
fn custom() -> Config {
    let mut config = Config::default();
    let g = &mut config.general;
    g.motd = "Lobby".to_owned();
    g.server_ip = Ipv4Addr::LOCALHOST;
    g.server_ipv6 = Ipv6Addr::LOCALHOST;
    g.enable_ipv6 = false;
    g.server_port = 19140;
    g.nethernet_port = 19141;
    g.public_ip = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    g.max_players = 100;
    g.xbox_auth = false;
    g.white_list = true;
    g.gamemode = GameMode::Creative;
    g.force_gamemode = true;
    g.difficulty = Difficulty::Hard;
    g.hardcore = true;
    g.pvp = false;
    g.hunger = false;
    g.experience = false;
    g.experience_orbs = false;
    g.fall_damage = false;
    g.drowning = false;
    g.armor = false;
    g.durability = false;
    g.beds = false;
    g.daylight_cycle = false;
    g.recipe_book = false;
    g.vanilla_recipes = false;
    g.education_items = false;
    g.item_despawn_ticks = 1200;
    g.sprint_critical_hits = true;
    g.break_time_check = false;
    g.movement_check = false;
    g.level_name = "lobby".to_owned();
    g.level_type = LevelType::Void;
    g.level_seed = Some(-42);
    g.view_distance = 8;
    g.save_player_data = false;
    g.backups = false;
    g.shutdown_message = "Bye".to_owned();
    g.resource_packs = vec!["a.mcpack".into(), "lobby/b.zip".into()];
    g.force_resources = true;
    g.log_level = LogLevel::Debug;
    let d = &mut config.developer;
    d.compression = Compression::Zlib;
    d.compression_threshold = 512;
    d.compression_level = 9;
    d.max_batch_bytes = MIB;
    d.enable_encryption = false;
    d.raknet = false;
    d.max_mtu_size = 1200;
    d.nethernet_tls_cert = Some("cert.pem".into());
    d.nethernet_tls_key = Some("key.pem".into());
    d.network_threads = Threads::Count(NonZeroU16::new(2).expect("non-zero"));
    d.cpu_threads = Threads::Count(NonZeroU16::new(3).expect("non-zero"));
    d.chunks_per_tick = 8;
    d.spawn_radius = 6;
    d.tick_radius = 5;
    d.random_tick_speed = 0;
    d.disable_block_ticking = vec!["minecraft:grass_block".to_owned()];
    d.slow_empty_worlds = true;
    d.lighting = Lighting::OnLoad;
    d.fixed_light_level = 7;
    d.item_pickup_interval = 4;
    d.hook_movement_packets = false;
    d.hook_player_input = false;
    d.save_interval_minutes = 5;
    d.integrity_check = IntegrityCheck::Never;
    d.console_input = false;
    let lobby = WorldSettings {
        view_distance: Some(6),
        chunks_per_tick: Some(2),
        spawn_radius: Some(3),
        tick_radius: Some(2),
        spawn: Some(BlockPos::new(i32::MIN, -60, i32::MAX)),
    };
    let arena = WorldSettings {
        tick_radius: Some(1),
        ..WorldSettings::default()
    };
    config.worlds.insert("lobby".to_owned(), lobby);
    config.worlds.insert("arena".to_owned(), arena);
    config
}

fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

/// Every key with values inside its rule and values just outside it.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one table lists the cases of every key"
)]
fn cases() -> Vec<(&'static str, Vec<String>, Vec<String>)> {
    let s = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let bools = || s(&["true", "false"]);
    vec![
        (
            "general.motd",
            vec![quoted("a"), quoted(&"m".repeat(256)), quoted("§aLobby")],
            vec![
                quoted(""),
                quoted(&"m".repeat(257)),
                quoted("a;b"),
                quoted(r"a\nb"),
                quoted(r"\u0007"),
            ],
        ),
        (
            "general.server-ip",
            s(&[r#""0.0.0.0""#, r#""127.0.0.1""#, r#""255.255.255.255""#]),
            s(&[r#""::""#, r#""1.2.3""#, r#""localhost""#, r#""""#]),
        ),
        (
            "general.server-ipv6",
            s(&[r#""::""#, r#""::1""#, r#""fe80::1""#]),
            s(&[r#""0.0.0.0""#, r#"":::""#, r#""""#]),
        ),
        ("general.enable-ipv6", bools(), vec![]),
        (
            "general.server-port",
            s(&["0", "65535"]),
            s(&["-1", "65536"]),
        ),
        (
            "general.nethernet-port",
            s(&["0", "65535"]),
            s(&["-1", "65536"]),
        ),
        (
            "general.public-ip",
            s(&[r#""""#, r#""203.0.113.5""#, r#""2001:db8::1""#]),
            s(&[r#""x""#, r#"" 1.2.3.4""#]),
        ),
        (
            "general.max-players",
            s(&["1", "10000"]),
            s(&["0", "10001"]),
        ),
        ("general.xbox-auth", bools(), vec![]),
        ("general.white-list", bools(), vec![]),
        (
            "general.gamemode",
            s(&[
                r#""survival""#,
                r#""creative""#,
                r#""adventure""#,
                r#""spectator""#,
            ]),
            s(&[r#""Survival""#, r#""hardcore""#, r#""""#]),
        ),
        ("general.force-gamemode", bools(), vec![]),
        (
            "general.difficulty",
            s(&[r#""peaceful""#, r#""easy""#, r#""normal""#, r#""hard""#]),
            s(&[r#""HARD""#, r#""extreme""#]),
        ),
        // `true` needs `difficulty = "hard"`; see `hardcore_needs_hard_difficulty`.
        ("general.hardcore", s(&["false"]), s(&["true"])),
        ("general.pvp", bools(), vec![]),
        ("general.hunger", bools(), vec![]),
        ("general.experience", bools(), vec![]),
        ("general.experience-orbs", bools(), vec![]),
        ("general.fall-damage", bools(), vec![]),
        ("general.drowning", bools(), vec![]),
        ("general.armor", bools(), vec![]),
        ("general.durability", bools(), vec![]),
        ("general.beds", bools(), vec![]),
        ("general.daylight-cycle", bools(), vec![]),
        ("general.recipe-book", bools(), vec![]),
        ("general.vanilla-recipes", bools(), vec![]),
        ("general.education-items", bools(), vec![]),
        (
            "general.item-despawn-ticks",
            s(&["1", "72000"]),
            s(&["0", "72001", "-1"]),
        ),
        ("general.sprint-critical-hits", bools(), vec![]),
        ("general.break-time-check", bools(), vec![]),
        ("general.movement-check", bools(), vec![]),
        (
            "general.level-name",
            vec![quoted("a"), quoted(&"w".repeat(64))],
            vec![
                quoted(""),
                quoted(&"w".repeat(65)),
                quoted("a;b"),
                quoted(r"a\tb"),
            ],
        ),
        (
            "general.level-type",
            s(&[r#""flat""#, r#""void""#]),
            s(&[r#""normal""#, r#""Flat""#]),
        ),
        (
            "general.level-seed",
            s(&[
                r#""""#,
                r#""0""#,
                r#""-9223372036854775808""#,
                r#""9223372036854775807""#,
            ]),
            s(&[
                r#""9223372036854775808""#,
                r#""abc""#,
                r#"" 1""#,
                r#""1.5""#,
            ]),
        ),
        ("general.view-distance", s(&["2", "64"]), s(&["1", "65"])),
        ("general.save-player-data", bools(), vec![]),
        ("general.backups", bools(), vec![]),
        (
            "general.shutdown-message",
            vec![quoted(""), quoted(&"x".repeat(1024)), quoted(r"two\nlines")],
            vec![quoted(&"x".repeat(1025))],
        ),
        (
            "general.resource-packs",
            s(&["[]", r#"["a.mcpack"]"#, r#"["a.zip", "b/c.mcpack"]"#]),
            s(&[r#"[""]"#, r#"["a.zip", ""]"#]),
        ),
        ("general.force-resources", bools(), vec![]),
        (
            "general.log-level",
            s(&[
                r#""off""#,
                r#""error""#,
                r#""warn""#,
                r#""info""#,
                r#""debug""#,
                r#""trace""#,
            ]),
            s(&[r#""Info""#, r#""warning""#, r#""""#, "true"]),
        ),
        (
            "developer.compression",
            s(&[r#""snappy""#, r#""zlib""#]),
            s(&[r#""none""#, r#""Zlib""#]),
        ),
        (
            "developer.compression-threshold",
            s(&["1", "65535"]),
            s(&["0", "65536", "-1"]),
        ),
        (
            "developer.compression-level",
            s(&["1", "9"]),
            s(&["0", "10", "256"]),
        ),
        (
            "developer.max-batch-bytes",
            s(&["1048576", "67108864"]),
            s(&["1048575", "67108865", "-1"]),
        ),
        ("developer.enable-encryption", bools(), vec![]),
        ("developer.raknet", bools(), vec![]),
        (
            "developer.max-mtu-size",
            s(&["576", "1492"]),
            s(&["575", "1493"]),
        ),
        // Each needs the other; see `tls_cert_and_key_go_together`.
        (
            "developer.nethernet-tls-cert",
            s(&[r#""""#]),
            s(&[r#""cert.pem""#]),
        ),
        (
            "developer.nethernet-tls-key",
            s(&[r#""""#]),
            s(&[r#""key.pem""#]),
        ),
        (
            "developer.network-threads",
            s(&[r#""auto""#, "1", "65535"]),
            s(&["0", "65536", r#""Auto""#, r#""2""#]),
        ),
        (
            "developer.cpu-threads",
            s(&[r#""auto""#, "1", "65535"]),
            s(&["0", "65536", r#""Auto""#, r#""2""#]),
        ),
        (
            "developer.chunks-per-tick",
            s(&["1", "256"]),
            s(&["0", "257"]),
        ),
        ("developer.spawn-radius", s(&["1", "64"]), s(&["0", "65"])),
        ("developer.tick-radius", s(&["1", "64"]), s(&["0", "65"])),
        (
            "developer.random-tick-speed",
            s(&["0", "4096"]),
            s(&["-1", "4097"]),
        ),
        (
            "developer.disable-block-ticking",
            s(&[
                "[]",
                r#"["minecraft:grass_block"]"#,
                r#"["my-pack.v2:deco/lamp_1"]"#,
            ]),
            s(&[
                r#"["grass_block"]"#,
                r#"["minecraft:"]"#,
                r#"[":grass"]"#,
                r#"["Minecraft:grass"]"#,
                r#"["a:b:c"]"#,
                r#"["minecraft:grass block"]"#,
            ]),
        ),
        ("developer.slow-empty-worlds", bools(), vec![]),
        // `true` and `false` are the values of older files; see `old_lighting_switch_still_loads`.
        (
            "developer.lighting",
            s(&[
                r#""dynamic""#,
                r#""on-load""#,
                r#""fixed""#,
                "true",
                "false",
            ]),
            s(&[r#""Dynamic""#, r#""onload""#, r#""""#, "1"]),
        ),
        (
            "developer.fixed-light-level",
            s(&["0", "15"]),
            s(&["-1", "16"]),
        ),
        (
            "developer.item-pickup-interval",
            s(&["1", "20"]),
            s(&["0", "21"]),
        ),
        ("developer.hook-movement-packets", bools(), vec![]),
        ("developer.hook-player-input", bools(), vec![]),
        (
            "developer.save-interval-minutes",
            s(&["1", "1440"]),
            s(&["0", "1441"]),
        ),
        (
            "developer.integrity-check",
            s(&[r#""always""#, r#""after-unclean-shutdown""#, r#""never""#]),
            s(&[r#""Always""#, r#""""#, "true"]),
        ),
        ("developer.console-input", bools(), vec![]),
    ]
}

#[test]
fn missing_file_is_created_with_the_documented_default_text() {
    let file = File::new("missing_file");
    let first = Config::default().load(&file.path).expect("loads");
    assert_eq!(first, Config::default());
    assert_eq!(read(&file.path), documented_default_file());
    assert!(!exists(&file.path.with_extension("toml.tmp")));
    let again = Config::default().load(&file.path).expect("loads again");
    assert_eq!(again, first);
}

#[test]
fn missing_file_reads_like_the_file_it_writes() {
    let file = File::new("missing_file_over_code");
    let first = custom().load(&file.path).expect("loads");
    // The default file sets every key; it has no `[worlds]` tables, so those set in code stay.
    let mut expected = Config::default();
    expected.worlds = custom().worlds;
    assert_eq!(first, expected);
    let again = custom().load(&file.path).expect("loads again");
    assert_eq!(again, first);
}

// pmmp: GameMode
#[test]
fn every_key_reaches_its_field() {
    let file = File::new("every_key_reaches_its_field");
    assert_eq!(file.load(CUSTOM_FILE).expect("loads"), custom());
}

#[test]
fn every_key_left_out_keeps_the_previous_value() {
    let file = File::new("every_key_left_out");
    let default = documented_default_file();
    let keys = key_lines(&default);
    assert_eq!(keys.len(), 66, "the default file has 66 keys");
    for (_, key, value) in &keys {
        let line = format!("{key} = {value}\n");
        let text = default.replacen(&line, "", 1);
        assert_ne!(text, default, "{key} is removed");
        assert_eq!(file.load(&text).expect("loads"), Config::default(), "{key}");
    }
    for text in ["", "[general]\n[developer]\n"] {
        assert_eq!(file.load_over(custom(), text).expect("loads"), custom());
    }
    let mut expected = custom();
    expected.general.pvp = true;
    let loaded = file.load_over(custom(), "[general]\npvp = true\n");
    assert_eq!(loaded.expect("loads"), expected);
}

#[test]
fn every_key_rejects_a_wrong_type_by_name() {
    let file = File::new("every_key_wrong_type");
    for (table, key, value) in key_lines(&documented_default_file()) {
        let path = format!("{table}.{key}");
        // `lighting` also takes the `true` and `false` of older files.
        let typed = if path == "developer.lighting" {
            "1"
        } else if value.starts_with('"') {
            "false"
        } else {
            "\"x\""
        };
        for wrong in ["1.5", "{ a = 1 }", "1979-05-27", typed] {
            let error = file
                .load(one_key(&path, wrong))
                .expect_err(&format!("{path} = {wrong}"));
            assert!(
                matches!(error, Error::Invalid { ref key, .. } if *key == path),
                "{path} = {wrong}: {error}"
            );
            assert!(error.to_string().contains(&format!("`{path}`")));
        }
    }
    let error = file.load(one_key("general.resource-packs", "[1]"));
    assert!(chain(&error.expect_err("integers")).contains("general.resource-packs"));
}

#[test]
fn every_key_accepts_its_edges_and_rejects_beyond() {
    let file = File::new("every_key_edges");
    let cases = cases();
    let mut listed: Vec<String> = cases.iter().map(|(k, _, _)| (*k).to_owned()).collect();
    let mut keys: Vec<String> = key_lines(&documented_default_file())
        .into_iter()
        .map(|(table, key, _)| format!("{table}.{key}"))
        .collect();
    listed.sort();
    keys.sort();
    assert_eq!(listed, keys, "every key has edge cases");
    for (path, accepted, rejected) in cases {
        for value in accepted {
            file.load(one_key(path, &value))
                .unwrap_or_else(|e| panic!("{path} = {value}: {}", chain(&e)));
        }
        for value in rejected {
            let error = file
                .load(one_key(path, &value))
                .expect_err(&format!("{path} = {value}"));
            assert!(chain(&error).contains(path), "{path} = {value}: {error}");
        }
    }
}

#[test]
fn range_errors_name_the_key_and_the_range() {
    let file = File::new("range_errors");
    let error = file.load(one_key("general.view-distance", "65"));
    assert_eq!(
        error.expect_err("65").to_string(),
        "`general.view-distance` = 65 is outside 2..=64"
    );
    let error = file.load(one_key("general.view-distance", "300"));
    assert_eq!(
        error.expect_err("300").to_string(),
        "`general.view-distance` = 300 is outside 2..=64"
    );
    let error = file.load(one_key("developer.compression-level", "10"));
    let text = chain(&error.expect_err("10"));
    assert!(text.contains("`developer.compression-level`") && text.contains("1..=9"));
    let error = file.load(one_key("developer.max-batch-bytes", "67108865"));
    assert!(chain(&error.expect_err("64 MiB + 1")).contains("1048576..=67108864"));
    let error = file.load(one_key("general.gamemode", "\"x\""));
    assert_eq!(
        error.expect_err("x").to_string(),
        r#"`general.gamemode` must be one of "survival", "creative", "adventure", "spectator""#
    );
}

#[test]
fn new_keys_default_to_the_behaviour_before_them() {
    let config = Config::default();
    let (g, d) = (&config.general, &config.developer);
    for on in [
        g.experience,
        g.experience_orbs,
        g.durability,
        g.education_items,
        g.fall_damage,
        g.drowning,
        g.armor,
        g.beds,
        g.daylight_cycle,
        g.recipe_book,
        g.vanilla_recipes,
        g.break_time_check,
        g.movement_check,
        g.backups,
        d.enable_encryption,
        d.hook_movement_packets,
        d.hook_player_input,
    ] {
        assert!(on);
    }
    assert!(!g.sprint_critical_hits && !d.slow_empty_worlds);
    assert_eq!(g.item_despawn_ticks, 6000);
    assert_eq!(g.log_level, LogLevel::Info);
    assert_eq!(g.shutdown_message, "The server has stopped");
    assert_eq!(d.lighting, Lighting::Dynamic);
    assert_eq!(d.fixed_light_level, 15);
    assert_eq!(d.item_pickup_interval, 1);
    assert_eq!(d.integrity_check, IntegrityCheck::Always);
    assert!(config.worlds.is_empty());
}

#[test]
fn old_lighting_switch_still_loads() {
    let file = File::new("old_lighting");
    let on = file
        .load(one_key("developer.lighting", "true"))
        .expect("true");
    assert_eq!(on.developer.lighting, Lighting::Dynamic);
    let off = file
        .load(one_key("developer.lighting", "false"))
        .expect("false");
    assert_eq!(off.developer.lighting, Lighting::Fixed);
    assert_eq!(
        off.developer.fixed_light_level, 15,
        "the old switch lit at 15"
    );
    let error = file.load(one_key("developer.lighting", "\"bright\""));
    assert_eq!(
        error.expect_err("bright").to_string(),
        r#"`developer.lighting` must be one of "dynamic", "on-load", "fixed""#
    );
}

#[test]
fn world_tables_set_values_of_one_world() {
    let file = File::new("world_tables");
    let config = file
        .load("[worlds.lobby]\nview-distance = 8\n[worlds.\"my world\"]\ntick-radius = 2\n")
        .expect("loads");
    assert_eq!(config.worlds.len(), 2);
    assert_eq!(config.worlds["lobby"].view_distance, Some(8));
    assert_eq!(config.worlds["lobby"].chunks_per_tick, None);
    assert_eq!(config.worlds["my world"].tick_radius, Some(2));
    // A key left out keeps the value set in code, as in the other tables.
    let mut code = Config::default();
    code.worlds.insert(
        "lobby".to_owned(),
        WorldSettings {
            chunks_per_tick: Some(9),
            ..WorldSettings::default()
        },
    );
    let merged = file
        .load_over(code, "[worlds.lobby]\nview-distance = 8\n")
        .expect("loads");
    assert_eq!(merged.worlds["lobby"].view_distance, Some(8));
    assert_eq!(merged.worlds["lobby"].chunks_per_tick, Some(9));
    file.load("[worlds]\n").expect("an empty worlds table");
}

#[test]
fn world_tables_reject_bad_values_by_key() {
    let file = File::new("world_tables_bad");
    for (text, key) in [
        (
            "[worlds.lobby]\nview-distance = 1\n",
            "worlds.lobby.view-distance",
        ),
        (
            "[worlds.lobby]\nview-distance = 65\n",
            "worlds.lobby.view-distance",
        ),
        (
            "[worlds.lobby]\nchunks-per-tick = 0\n",
            "worlds.lobby.chunks-per-tick",
        ),
        (
            "[worlds.lobby]\nchunks-per-tick = 257\n",
            "worlds.lobby.chunks-per-tick",
        ),
        (
            "[worlds.lobby]\nspawn-radius = 0\n",
            "worlds.lobby.spawn-radius",
        ),
        (
            "[worlds.lobby]\ntick-radius = 65\n",
            "worlds.lobby.tick-radius",
        ),
        (
            "[worlds.lobby]\ntick-radius = \"4\"\n",
            "worlds.lobby.tick-radius",
        ),
        (
            "[worlds.lobby]\nview-distance = 1.5\n",
            "worlds.lobby.view-distance",
        ),
        ("[worlds.lobby]\nspawn = [0, 64]\n", "worlds.lobby.spawn"),
        (
            "[worlds.lobby]\nspawn = [0, 64, 0, 0]\n",
            "worlds.lobby.spawn",
        ),
        (
            "[worlds.lobby]\nspawn = [0, 64.5, 0]\n",
            "worlds.lobby.spawn",
        ),
        ("[worlds.lobby]\nspawn = \"0 64 0\"\n", "worlds.lobby.spawn"),
        (
            "[worlds.lobby]\nspawn = [0, 2147483648, 0]\n",
            "worlds.lobby.spawn",
        ),
        ("[worlds]\nlobby = 5\n", "worlds.lobby"),
        ("worlds = 5\n", "worlds"),
    ] {
        let error = file.load(text).expect_err(key);
        assert!(
            matches!(&error, Error::Invalid { key: k, .. } | Error::Range { key: k, .. } if k == key),
            "{text}: {error}"
        );
    }
    let error = file
        .load("[worlds.lobby]\nmotd = \"x\"\n")
        .expect_err("unknown key");
    assert_eq!(error.to_string(), "unknown key `worlds.lobby.motd`");
    let mut code = Config::default();
    code.worlds.insert(
        "lobby".to_owned(),
        WorldSettings {
            spawn_radius: Some(65),
            ..WorldSettings::default()
        },
    );
    let error = Server::builder()
        .config(code)
        .build()
        .expect_err("set in code");
    assert!(chain_of(&error).contains("`worlds.lobby.spawn-radius` = 65 is outside 1..=64"));
}

#[test]
fn world_tables_must_name_a_declared_world() {
    let file = File::new("world_names");
    let text = "[worlds.world]\ntick-radius = 2\n[worlds.lobby]\nview-distance = 8\n";
    let config = file.load(text).expect("names are checked at build");
    let error = Server::builder()
        .config(config.clone())
        .build()
        .expect_err("lobby is not declared");
    assert!(
        chain_of(&error).contains("`[worlds.lobby]` names no declared world"),
        "{}",
        chain_of(&error)
    );
    Server::builder()
        .config(config)
        .world("lobby", Kind::Temporary, Flat)
        .build()
        .expect("the default world and lobby are declared");
}

/// Returns a build error and its sources, joined with ": ".
fn chain_of(error: &anchor::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[test]
fn hardcore_needs_hard_difficulty() {
    let file = File::new("hardcore");
    let config = file
        .load("[general]\nhardcore = true\ndifficulty = \"hard\"\n")
        .expect("hard");
    assert!(config.general.hardcore);
    for difficulty in ["peaceful", "easy", "normal"] {
        let text = format!("[general]\nhardcore = true\ndifficulty = \"{difficulty}\"\n");
        let error = file.load(text).expect_err(difficulty);
        assert!(error.to_string().contains("`general.hardcore`"));
    }
    let mut code = Config::default();
    code.general.difficulty = Difficulty::Hard;
    let loaded = file.load_over(code, "[general]\nhardcore = true\n");
    assert!(loaded.expect("hard from code").general.hardcore);
}

#[test]
fn tls_cert_and_key_go_together() {
    let file = File::new("tls");
    let both = "[developer]\nnethernet-tls-cert = \"c.pem\"\nnethernet-tls-key = \"k.pem\"\n";
    let config = file.load(both).expect("both set");
    assert_eq!(config.developer.nethernet_tls_cert, Some("c.pem".into()));
    assert_eq!(config.developer.nethernet_tls_key, Some("k.pem".into()));
    let error = file.load(one_key("developer.nethernet-tls-cert", "\"c.pem\""));
    assert!(
        error
            .expect_err("cert only")
            .to_string()
            .contains("nethernet-tls-key")
    );
    let error = file.load(one_key("developer.nethernet-tls-key", "\"k.pem\""));
    assert!(
        error
            .expect_err("key only")
            .to_string()
            .contains("nethernet-tls-cert")
    );
}

#[test]
fn unknown_keys_are_rejected_by_name() {
    let file = File::new("unknown_keys");
    for (text, key) in [
        ("[general]\nmotdd = \"x\"\n", "general.motdd"),
        (
            "[developer]\nview-distance = 4\n",
            "developer.view-distance",
        ),
        ("motd = \"x\"\n", "motd"),
        ("[[plugins]]\nname = \"x\"\n", "plugins"),
    ] {
        let error = file.load(text).expect_err(key);
        assert!(
            matches!(error, Error::UnknownKey(ref k) if k == key),
            "{error}"
        );
        assert_eq!(error.to_string(), format!("unknown key `{key}`"));
    }
    let error = file.load("general = 5\n").expect_err("not a table");
    assert_eq!(error.to_string(), "`general` must be a table");
    file.load("[plugin]\nanswer = 42\n")
        .expect("unknown tables are rejected when the server is built");
}

#[test]
fn renamed_keys_are_rejected_with_their_new_name() {
    let file = File::new("renamed_keys");
    let error = file
        .load("[general]\nexperience-bottles = false\n")
        .expect_err("renamed key");
    assert!(
        matches!(
            error,
            Error::RenamedKey { ref key, renamed: "general.experience-orbs" }
                if key == "general.experience-bottles"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "`general.experience-bottles` was renamed to `general.experience-orbs`"
    );
    // The old name in another table is just unknown.
    let error = file
        .load("[developer]\nexperience-bottles = false\n")
        .expect_err("unknown key");
    assert!(
        matches!(error, Error::UnknownKey(ref k) if k == "developer.experience-bottles"),
        "{error:?}"
    );
}

#[test]
fn oversized_file_is_rejected_before_parsing() {
    let file = File::new("oversized");
    let line = format!("#{}\n", "x".repeat(1022));
    let full = line.repeat(1024);
    assert_eq!(full.len(), MIB);
    file.load(&full).expect("1 MiB of comments");
    let error = file.load(full + "\n").expect_err("1 MiB + 1");
    assert!(matches!(error, Error::TooLarge { .. }), "{error}");
}

#[test]
fn deep_nesting_is_a_syntax_error_not_a_stack_overflow() {
    let file = File::new("deep_nesting");
    let nested = |prefix: &str, unit: &str, suffix: &str| {
        let count = (MIB - prefix.len() - suffix.len()) / unit.len();
        format!("{prefix}{}{suffix}", unit.repeat(count))
    };
    for text in [
        nested("[general]\nmotd = ", "[", ""),
        nested("[general]\nmotd = ", "{a=", ""),
        nested("[general]\nmotd = ", "[{a=", ""),
        nested("a", ".a", " = 1\n"),
        nested("[a", ".a", "]\n"),
        nested("[[a", ".a", "]]\n"),
    ] {
        let error = file.load(&text).expect_err("too deep");
        assert!(matches!(error, Error::Syntax { .. }), "{error}");
    }
}

#[test]
fn syntax_errors_report_line_and_column() {
    let file = File::new("syntax");
    let error = file
        .load("[general]\nmotd = \"x\n")
        .expect_err("unclosed string");
    assert!(
        matches!(
            error,
            Error::Syntax {
                position: Some((2, _)),
                ..
            }
        ),
        "{error}"
    );
    let error = file.load("[general]\npvp = true\npvp = false\n");
    let error = error.expect_err("duplicate key");
    assert!(
        matches!(
            error,
            Error::Syntax {
                position: Some((3, 1)),
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .starts_with("invalid toml at line 3, column 1: ")
    );
    let error = file
        .load(b"[general]\nmotd = \"\xff\"\n")
        .expect_err("not UTF-8");
    assert!(
        matches!(
            error,
            Error::Syntax {
                position: Some((2, 9)),
                ..
            }
        ),
        "{error}"
    );
    file.load("\u{feff}[general]\npvp = false\n")
        .expect("a byte order mark is allowed");
}

#[test]
fn unreadable_or_unwritable_paths_are_errors() {
    let file = File::new("paths");
    let dir = file.path.parent().expect("a directory").to_owned();
    let error = Config::default().load(&dir).expect_err("a directory");
    assert!(matches!(error, Error::Read { .. }), "{error}");
    assert!(error.source().is_some());
    let missing = dir.join("no-such-dir").join("anchor.toml");
    let error = Config::default().load(&missing).expect_err("no directory");
    assert!(matches!(error, Error::Write { .. }), "{error}");
    assert!(!exists(&missing));
}

#[test]
fn update_puts_each_missing_key_back_where_the_default_file_has_it() {
    let file = File::new("update_each_key");
    let default = documented_default_file();
    let keys: Vec<_> = blocks(&default)
        .into_iter()
        .filter(|(path, _)| !path.starts_with('['))
        .collect();
    assert_eq!(keys.len(), 66);
    for (path, block) in keys {
        let text = default.replacen(&block, "", 1);
        assert_ne!(text, default, "{path}");
        let (update, updated) = write_and_update(&file, &text);
        assert_eq!(update.added(), std::slice::from_ref(&path));
        assert!(update.renamed().is_empty());
        assert_eq!(updated, default, "{path}");
    }
}

#[test]
fn update_leaves_a_current_file_as_it_is() {
    let file = File::new("update_current");
    let (update, updated) = write_and_update(&file, &documented_default_file());
    assert!(update.is_empty());
    assert_eq!(updated, documented_default_file());
}

#[test]
fn update_adds_missing_tables_at_the_end() {
    let file = File::new("update_tables");
    let default = documented_default_file();
    let blocks = blocks(&default);
    // The header and keys of `table` with their comment lines, as the default file has them.
    let table_text = |table: &str| -> String {
        let (header, prefix) = (format!("[{table}]"), format!("{table}."));
        let table = blocks
            .iter()
            .filter(|(p, _)| *p == header || p.starts_with(&prefix));
        table.map(|(_, block)| block.as_str()).collect()
    };
    let (general, developer) = (table_text("general"), table_text("developer"));
    let keys = blocks.iter().map(|(path, _)| path);
    let keys: Vec<&String> = keys.filter(|p| !p.starts_with('[')).collect();

    let (update, updated) = write_and_update(&file, "");
    assert_eq!(update.added().iter().collect::<Vec<_>>(), keys);
    assert_eq!(updated, format!("{general}\n{developer}"));

    // The last line has no line ending; the missing keys of its table come before the new table.
    let (update, updated) = write_and_update(&file, "[general]\nmotd = \"x\"");
    let (_, rest) = general
        .split_once("motd = \"Anchor Server\"\n")
        .expect("motd's line");
    assert_eq!(
        updated,
        format!("[general]\nmotd = \"x\"\n{rest}\n{developer}")
    );
    assert_eq!(update.added().len(), 65);

    let (update, updated) = write_and_update(&file, "[developer]\nraknet = false\n");
    assert!(updated.starts_with("[developer]\n"));
    assert!(updated.ends_with(&format!("\nconsole-input = true\n\n{general}")));
    assert_eq!(update.added().len(), 65);
}

#[test]
fn update_keeps_the_rest_of_the_file_byte_for_byte() {
    let file = File::new("update_keeps");
    // A byte order mark, CRLF line endings, comments after values and headers, a multi-line
    // string with a line like a header, a multi-line array, a world table, an unknown table
    // and no line ending at the end.
    let text = "\u{feff}# my server\r\n[general]\r\nmotd = \"Lobby\" # shown in the list\r\n\
                shutdown-message = \"\"\"\r\n[bye]\r\n\"\"\"\r\n\r\n[developer] # tuning\r\n\
                disable-block-ticking = [\r\n    \"minecraft:grass_block\",\r\n]\r\n\r\n\
                [worlds.lobby]\r\nview-distance = 6\r\n[plugin]\r\nanswer = 42";
    let (update, updated) = write_and_update(&file, text);
    assert_eq!(update.added().len(), 63);
    for kept in [
        "general.motd",
        "general.shutdown-message",
        "developer.disable-block-ticking",
    ] {
        assert!(!update.added().iter().any(|p| p == kept), "{kept}");
    }
    assert!(!updated.replace("\r\n", "").contains('\n'), "only CRLF");
    let default = blocks(&documented_default_file());
    let mut stripped = updated.clone();
    for path in update.added() {
        let (_, block) = default.iter().find(|(p, _)| p == path).expect("a key");
        let block = block.replace('\n', "\r\n");
        assert!(stripped.contains(&block), "{path}");
        stripped = stripped.replacen(&block, "", 1);
    }
    assert_eq!(stripped, text);
    // A key goes after the key before it in the default file, not into the string.
    assert!(updated.contains("\"\"\"\r\n[bye]\r\n\"\"\"\r\n# Resource pack files"));
    assert!(updated.contains("[developer] # tuning\r\n# Network compression"));
}

#[test]
fn update_renames_old_keys_and_keeps_their_value() {
    let file = File::new("update_renamed");
    for old in ["experience-bottles", "\"experience-bottles\""] {
        let text = format!("[general]\nhunger = false\n{old} = false # no orbs\n");
        write(&file.path, text.as_bytes());
        let update = Config::update_file(&file.path).expect("updates");
        assert_eq!(
            update.renamed(),
            [("general.experience-bottles", "general.experience-orbs")]
        );
        let added = update.added();
        assert_eq!(added.len(), 64);
        assert!(!added.iter().any(|p| p == "general.experience-orbs"));
        let updated = read(&file.path);
        assert!(
            updated.contains(
                "hunger = false\n# Gaining experience points and levels. false also means no \
                 experience orbs.\nexperience = true\nexperience-orbs = false # no orbs\n\
                 fall-damage = true\n"
            ),
            "{updated}"
        );
        let config = Config::default().load(&file.path).expect("loads");
        assert!(!config.general.experience_orbs && !config.general.hunger);
        assert!(Config::update_file(&file.path).expect("again").is_empty());
    }

    // With both names the old key stays, and the update stops on it.
    let text = "[general]\nexperience-bottles = false\nexperience-orbs = true\n";
    write(&file.path, text.as_bytes());
    let error = Config::update_file(&file.path).expect_err("both names");
    assert!(matches!(error, Error::RenamedKey { .. }), "{error}");
    assert_eq!(read(&file.path), text);
}

#[test]
fn update_refuses_a_file_it_cannot_update_and_leaves_it() {
    let file = File::new("update_refused");
    let error = Config::update_file(&file.path).expect_err("no file");
    assert!(
        matches!(&error, Error::Read { source, .. } if source.kind() == ErrorKind::NotFound),
        "{error}"
    );
    assert!(!exists(&file.path));

    let line = format!("#{}\n", "x".repeat(1022));
    let full = format!("[general]\n{}", line.repeat(1023));
    let full = format!("{full}{}\n", "#".repeat(1024 * 1024 - full.len() - 1));
    assert_eq!(full.len(), MIB);
    for (text, expected) in [
        ("[general]\nmotdd = 1\n", "unknown key `general.motdd`"),
        (
            "[general]\nview-distance = 1\n",
            "`general.view-distance` = 1 is outside 2..=64",
        ),
        ("[general]\nmotd = \n", "invalid toml at line 2"),
        (
            "general = { pvp = false }\n",
            "keys cannot be added to `general`: write it as a `[general]` table",
        ),
        (
            "general.pvp = false\n",
            "keys cannot be added to `general`: write it as a `[general]` table",
        ),
        (full.as_str(), "is larger than 1048576 bytes"),
    ] {
        write(&file.path, text.as_bytes());
        let error = Config::update_file(&file.path).expect_err(expected);
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(read(&file.path), text);
        assert!(!exists(&file.path.with_extension("toml.tmp")));
    }
}
