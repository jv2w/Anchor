//! The settings file through the public API: the default file, every key's defaults, types and
//! ranges, unknown keys, and file handling.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::error::Error as _;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anchor::config::{
    CriticalHits, Difficulty, Error, GameMode, IntegrityCheck, LevelType, Lighting, LogLevel,
    Threads, Update, WorldSettings,
};
use anchor::protocol::compression::Compression;
use anchor::world::{BORDER, BlockPos, Flat, Kind};
use anchor::{Config, Server};

/// `.claude/docs/config.md`, built in so that `cargo xtask check` reruns these tests when it
/// changes.
const CONFIG_DOC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/.claude/docs/config.md"
));

/// A file that sets every key to a value other than its default.
const CUSTOM_FILE: &str = r#"[general]
motd = "Lobby"
server-ip = "127.0.0.1"
server-ipv6 = "::1"
enable-ipv6 = false
server-port = 19140
public-ip = "203.0.113.7"
max-players = 100
xbox-auth = false
white-list = true
gamemode = "creative"
force-gamemode = true
difficulty = "hard"
hardcore = true
pvp = false
food = false
xp = false
xp-orbs = false
fall-damage = false
drowning = false
armor = false
durability = false
daylight-cycle = false
item-despawn-ticks = 1200
critical-hits = "always"
break-time-check = false
movement-check = false
show-coordinates = true
locator-bar = false
immediate-respawn = true
show-death-messages = true
level-name = "lobby"
level-type = "void"
level-seed = "-42"
view-distance = 8
save-player-data = false
backups = false
max-backups = 7
shutdown-message = "Bye"
resource-packs = ["a.mcpack", "lobby/b.zip"]
force-resources = true
log-level = "debug"

[developer]
compression = "zlib"
compression-threshold = 512
compression-level = 9
max-batch-bytes = 8388608
enable-encryption = false
raknet = false
raknet-cookies = true
max-mtu-size = 1200
nethernet-tls-cert = "cert.pem"
nethernet-tls-key = "key.pem"
network-threads = 2
cpu-threads = 3
chunks-per-tick = 8
spawn-radius = 6
tick-radius = 5
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
spawn = [-30000000, -60, 29999999]

[worlds.arena]
tick-radius = 1
"#;

/// The number of keys in the default file. The library's unit tests pin it too (in
/// `src/config.rs`), since this crate cannot reach their constant.
const DEFAULT_KEY_COUNT: usize = 65;

/// The default file, as [`default_file`] reads it once.
static DEFAULT_TEXT: OnceLock<String> = OnceLock::new();

/// The bytes of one mebibyte.
const MIB: usize = 1024 * 1024;

/// The keys whose range `.claude/docs/config.md` writes as `A to B`, in its order.
const RANGED_KEYS: [&str; 23] = [
    "general.motd",
    "general.server-port",
    "general.max-players",
    "general.item-despawn-ticks",
    "general.level-name",
    "general.view-distance",
    "general.max-backups",
    "developer.compression-threshold",
    "developer.compression-level",
    "developer.max-batch-bytes",
    "developer.max-mtu-size",
    "developer.network-threads",
    "developer.cpu-threads",
    "developer.chunks-per-tick",
    "developer.spawn-radius",
    "developer.tick-radius",
    "developer.fixed-light-level",
    "developer.item-pickup-interval",
    "developer.save-interval-minutes",
    "worlds.lobby.view-distance",
    "worlds.lobby.chunks-per-tick",
    "worlds.lobby.spawn-radius",
    "worlds.lobby.tick-radius",
];

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

#[test]
fn missing_file_is_created_with_the_defaults() {
    let file = File::new("missing_file");
    let first = Config::default().load(&file.path).expect("loads");
    assert_eq!(first, Config::default());
    assert!(exists(&file.path));
    assert!(!exists(&file.path.with_extension("toml.tmp")));
    let again = Config::default().load(&file.path).expect("loads again");
    assert_eq!(again, first);
}

/// The config [`CUSTOM_FILE`] describes.
fn custom() -> Config {
    let mut config = Config::default();
    let g = &mut config.general;
    g.motd = "Lobby".to_owned();
    g.server_ip = Ipv4Addr::LOCALHOST;
    g.server_ipv6 = Ipv6Addr::LOCALHOST;
    g.enable_ipv6 = false;
    g.server_port = 19_140;
    g.public_ip = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    g.max_players = 100;
    g.xbox_auth = false;
    g.white_list = true;
    g.gamemode = GameMode::Creative;
    g.force_gamemode = true;
    g.difficulty = Difficulty::Hard;
    g.hardcore = true;
    g.pvp = false;
    g.food = false;
    g.xp = false;
    g.xp_orbs = false;
    g.fall_damage = false;
    g.drowning = false;
    g.armor = false;
    g.durability = false;
    g.daylight_cycle = false;
    g.item_despawn_ticks = 1200;
    g.critical_hits = CriticalHits::Always;
    g.break_time_check = false;
    g.movement_check = false;
    g.show_coordinates = true;
    g.locator_bar = false;
    g.immediate_respawn = true;
    g.show_death_messages = true;
    g.level_name = "lobby".to_owned();
    g.level_type = LevelType::Void;
    g.level_seed = Some(-42);
    g.view_distance = 8;
    g.save_player_data = false;
    g.backups = false;
    g.max_backups = 7;
    g.shutdown_message = "Bye".to_owned();
    g.resource_packs = vec![PathBuf::from("a.mcpack"), PathBuf::from("lobby/b.zip")];
    g.force_resources = true;
    g.log_level = LogLevel::Debug;
    let d = &mut config.developer;
    d.compression = Compression::Zlib;
    d.compression_threshold = 512;
    d.compression_level = 9;
    d.max_batch_bytes = 8 * MIB;
    d.enable_encryption = false;
    d.raknet = false;
    d.raknet_cookies = true;
    d.max_mtu_size = 1200;
    d.nethernet_tls_cert = Some(PathBuf::from("cert.pem"));
    d.nethernet_tls_key = Some(PathBuf::from("key.pem"));
    d.network_threads = Threads::Count(NonZeroU16::new(2).expect("non-zero"));
    d.cpu_threads = Threads::Count(NonZeroU16::new(3).expect("non-zero"));
    d.chunks_per_tick = 8;
    d.spawn_radius = 6;
    d.tick_radius = 5;
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
        spawn: Some(BlockPos::new(-BORDER, -60, BORDER - 1)),
    };
    let arena = WorldSettings {
        tick_radius: Some(1),
        ..WorldSettings::default()
    };
    config.worlds.insert("lobby".to_owned(), lobby);
    config.worlds.insert("arena".to_owned(), arena);
    config
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

// api: GameMode
#[test]
fn every_key_reaches_its_field() {
    let file = File::new("every_key_reaches_its_field");
    assert_eq!(file.load(CUSTOM_FILE).expect("loads"), custom());
}

/// The `(table, key, value)` of every key line of a settings file.
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

/// The default file, as `Config::load` writes it where there is no file.
fn default_file() -> String {
    DEFAULT_TEXT
        .get_or_init(|| {
            let file = File::new("default_file");
            Config::default()
                .load(&file.path)
                .expect("writes the default file");
            read(&file.path)
        })
        .clone()
}

#[test]
fn every_key_left_out_keeps_the_previous_value() {
    let file = File::new("every_key_left_out");
    let default = default_file();
    let keys = key_lines(&default);
    assert_eq!(
        keys.len(),
        DEFAULT_KEY_COUNT,
        "the default file's key count"
    );
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

/// A file that sets only `path` (`table.key`) to `value`.
fn one_key(path: &str, value: &str) -> String {
    let (table, key) = path.rsplit_once('.').expect("a dotted key");
    format!("[{table}]\n{key} = {value}\n")
}

/// The error and its sources, joined with ": ".
fn chain(error: &dyn std::error::Error) -> String {
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
fn every_key_rejects_a_wrong_type_by_name() {
    let file = File::new("every_key_wrong_type");
    for (table, key, value) in key_lines(&default_file()) {
        let path = format!("{table}.{key}");
        let typed = if value.starts_with('"') {
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

fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

/// The keys with values inside their rule and values outside it, besides the edges of a range
/// [`documented_ranges`] gives.
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
            vec![quoted("§aLobby")],
            vec![quoted("a;b"), quoted(r"a\nb"), quoted(r"\u0007")],
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
        ("general.enable-ipv6", bools(), Vec::new()),
        (
            "general.public-ip",
            s(&[r#""""#, r#""203.0.113.5""#, r#""2001:db8::1""#]),
            s(&[r#""x""#, r#"" 1.2.3.4""#]),
        ),
        ("general.xbox-auth", bools(), Vec::new()),
        ("general.white-list", bools(), Vec::new()),
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
        ("general.force-gamemode", bools(), Vec::new()),
        (
            "general.difficulty",
            s(&[r#""peaceful""#, r#""easy""#, r#""normal""#, r#""hard""#]),
            s(&[r#""HARD""#, r#""extreme""#]),
        ),
        // `true` needs `difficulty = "hard"`; see `hardcore_needs_hard_difficulty`.
        ("general.hardcore", s(&["false"]), s(&["true"])),
        ("general.pvp", bools(), Vec::new()),
        ("general.food", bools(), Vec::new()),
        ("general.xp", bools(), Vec::new()),
        ("general.xp-orbs", bools(), Vec::new()),
        ("general.fall-damage", bools(), Vec::new()),
        ("general.drowning", bools(), Vec::new()),
        ("general.armor", bools(), Vec::new()),
        ("general.durability", bools(), Vec::new()),
        ("general.daylight-cycle", bools(), Vec::new()),
        ("general.item-despawn-ticks", Vec::new(), s(&["-1"])),
        (
            "general.critical-hits",
            s(&[r#""off""#, r#""sprint""#, r#""always""#]),
            s(&[r#""Sprint""#, "true", r#""""#]),
        ),
        ("general.break-time-check", bools(), Vec::new()),
        ("general.movement-check", bools(), Vec::new()),
        ("general.show-coordinates", bools(), Vec::new()),
        ("general.locator-bar", bools(), Vec::new()),
        ("general.immediate-respawn", bools(), Vec::new()),
        ("general.show-death-messages", bools(), Vec::new()),
        (
            "general.level-name",
            Vec::new(),
            vec![quoted("a;b"), quoted(r"a\tb"), quoted("con"), quoted("a/b")],
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
        ("general.save-player-data", bools(), Vec::new()),
        ("general.backups", bools(), Vec::new()),
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
        ("general.force-resources", bools(), Vec::new()),
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
        ("developer.compression-threshold", Vec::new(), s(&["-1"])),
        ("developer.compression-level", Vec::new(), s(&["256"])),
        (
            "developer.max-batch-bytes",
            Vec::new(),
            s(&["1048576", "-1"]),
        ),
        ("developer.enable-encryption", bools(), Vec::new()),
        ("developer.raknet", bools(), Vec::new()),
        ("developer.raknet-cookies", bools(), Vec::new()),
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
            s(&[r#""auto""#]),
            s(&[r#""Auto""#, r#""2""#]),
        ),
        (
            "developer.cpu-threads",
            s(&[r#""auto""#]),
            s(&[r#""Auto""#, r#""2""#]),
        ),
        ("developer.slow-empty-worlds", bools(), Vec::new()),
        (
            "developer.lighting",
            s(&[r#""dynamic""#, r#""on-load""#, r#""fixed""#]),
            s(&[r#""Dynamic""#, r#""onload""#, r#""""#, "1", "true", "false"]),
        ),
        ("developer.hook-movement-packets", bools(), Vec::new()),
        ("developer.hook-player-input", bools(), Vec::new()),
        (
            "developer.integrity-check",
            s(&[r#""always""#, r#""after-unclean-shutdown""#, r#""never""#]),
            s(&[r#""Always""#, r#""""#, "true"]),
        ),
        ("developer.console-input", bools(), Vec::new()),
    ]
}

/// A key whose range `.claude/docs/config.md` writes as `A to B`.
struct DocumentedRange {
    /// The key's path, `table.key`, with `lobby` as the name of a world.
    path: String,
    /// Whether the value is text and the range its length in bytes.
    is_text: bool,
    low: i64,
    high: i64,
}

/// The ranges of the rows of the key tables of `.claude/docs/config.md` that write one as
/// `A to B`; those rows must be exactly the [`RANGED_KEYS`].
fn documented_ranges() -> Vec<DocumentedRange> {
    let mut table = String::new();
    let mut range_column = None;
    let mut ranges = Vec::new();
    for line in CONFIG_DOC.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            table = heading
                .split_once("`[")
                .and_then(|(_, rest)| rest.split_once("]`"))
                .map_or_else(String::new, |(name, _)| name.replace("<name>", "lobby"));
            range_column = None;
            continue;
        }
        if table.is_empty() || !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells[0] == "Key" {
            range_column = cells.iter().position(|cell| cell.starts_with("Range"));
            continue;
        }
        let column = range_column.expect("a key table starts with a header row with a range");
        let Some((before, after)) = cells[column].split_once(" to ") else {
            continue;
        };
        let bound = |word: Option<&str>| word.and_then(|word| word.parse::<i64>().ok());
        // Prose such as "relative to" is no range; the check of the paths below catches a range
        // this skips.
        let (Some(low), Some(high)) = (
            bound(before.split_whitespace().next_back()),
            bound(after.split_whitespace().next()),
        ) else {
            continue;
        };
        let path = format!("{table}.{}", cells[0].trim_matches('`'));
        ranges.push(DocumentedRange {
            path,
            is_text: cells[1] == "string",
            low,
            high,
        });
    }
    let paths: Vec<&str> = ranges.iter().map(|range| range.path.as_str()).collect();
    assert_eq!(paths, RANGED_KEYS, "the rows with an `A to B` range");
    ranges
}

/// Text of `bytes` bytes, in two-byte characters but for an odd last byte, so that a length
/// counted in characters fails the edges of a byte range.
fn text_of_bytes(bytes: i64) -> String {
    let bytes = usize::try_from(bytes).expect("a length");
    "é".repeat(bytes / 2) + &"m".repeat(bytes % 2)
}

/// Every key accepts the edges of its documented range and rejects the values just beyond them,
/// naming the key, and passes its other cases.
#[test]
fn every_key_accepts_its_edges_and_rejects_beyond() {
    let file = File::new("every_key_edges");
    let cases = cases();
    let ranges = documented_ranges();
    let mut covered: Vec<&str> = cases.iter().map(|(path, _, _)| *path).collect();
    covered.extend(
        ranges
            .iter()
            .map(|range| range.path.as_str())
            .filter(|path| !path.starts_with("worlds.")),
    );
    covered.sort_unstable();
    covered.dedup();
    let mut keys: Vec<String> = key_lines(&default_file())
        .into_iter()
        .map(|(table, key, _)| format!("{table}.{key}"))
        .collect();
    keys.sort();
    assert_eq!(covered, keys, "every key has edge cases");
    for range in ranges {
        let path = range.path.as_str();
        let value = |n: i64| {
            if range.is_text {
                quoted(&text_of_bytes(n))
            } else {
                n.to_string()
            }
        };
        for n in [range.low, range.high] {
            file.load(one_key(path, &value(n)))
                .unwrap_or_else(|e| panic!("{path} = {n} is documented: {}", chain(&e)));
        }
        for n in [range.low - 1, range.high + 1] {
            let error = file
                .load(one_key(path, &value(n)))
                .expect_err(&format!("{path} = {n} is outside the documented range"));
            assert!(chain(&error).contains(path), "{path} = {n}: {error}");
        }
    }
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
        g.xp,
        g.xp_orbs,
        g.durability,
        g.fall_damage,
        g.drowning,
        g.armor,
        g.daylight_cycle,
        g.break_time_check,
        g.movement_check,
        g.locator_bar,
        g.backups,
        d.enable_encryption,
        d.hook_movement_packets,
        d.hook_player_input,
    ] {
        assert!(on);
    }
    assert!(!d.raknet_cookies);
    assert!(!d.slow_empty_worlds);
    assert!(!g.show_coordinates);
    assert!(!g.immediate_respawn);
    assert!(!g.show_death_messages);
    assert_eq!(g.critical_hits, CriticalHits::Sprint);
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
fn lighting_takes_only_its_names() {
    let file = File::new("lighting_names");
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
        (
            "[worlds.lobby]\nspawn = [30000000, 64, 0]\n",
            "worlds.lobby.spawn",
        ),
        (
            "[worlds.lobby]\nspawn = [0, 64, -30000001]\n",
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
    assert!(chain(&error).contains("`worlds.lobby.spawn-radius` = 65 is outside 1..=64"));
    let mut code = Config::default();
    code.worlds.insert(
        "lobby".to_owned(),
        WorldSettings {
            spawn: Some(BlockPos::new(BORDER, 64, 0)),
            ..WorldSettings::default()
        },
    );
    let error = Server::builder()
        .config(code)
        .build()
        .expect_err("a spawn outside the border, set in code");
    assert!(chain(&error).contains("`worlds.lobby.spawn` must be [x, y, z]"));
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
        chain(&error).contains("`[worlds.lobby]` names no declared world"),
        "{}",
        chain(&error)
    );
    Server::builder()
        .config(config)
        .world("lobby", Kind::Temporary, Flat)
        .build()
        .expect("the default world and lobby are declared");
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
fn an_ipv6_public_address_needs_ipv6_listening() {
    let file = File::new("public_ipv6");
    let text = "[general]\nenable-ipv6 = false\npublic-ip = \"2001:db8::1\"\n";
    let error = file.load(text).expect_err("IPv6 listening is off");
    assert!(error.to_string().contains("`general.public-ip`"), "{error}");
    file.load("[general]\nenable-ipv6 = false\npublic-ip = \"203.0.113.5\"\n")
        .expect("an IPv4 address needs no IPv6 socket");
    file.load("[general]\npublic-ip = \"2001:db8::1\"\n")
        .expect("IPv6 listening is on by default");
}

#[test]
fn a_level_name_that_is_no_file_name_fails_the_build_naming_the_key() {
    let mut config = Config::default();
    config.general.level_name = "a/b".to_owned();
    let error = Server::builder()
        .config(config)
        .build()
        .expect_err("no file name");
    assert!(
        chain(&error).contains("`general.level-name` must be"),
        "{}",
        chain(&error)
    );
}

#[test]
fn tls_cert_and_key_go_together() {
    let file = File::new("tls");
    let both = "[developer]\nnethernet-tls-cert = \"c.pem\"\nnethernet-tls-key = \"k.pem\"\n";
    let config = file.load(both).expect("both set");
    assert_eq!(
        config.developer.nethernet_tls_cert,
        Some(PathBuf::from("c.pem"))
    );
    assert_eq!(
        config.developer.nethernet_tls_key,
        Some(PathBuf::from("k.pem"))
    );
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

/// The blocks of a settings file: each key line, by path (`table.key`), and each table header,
/// by `[table]`, with the comment lines right above it.
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

#[test]
fn update_puts_each_missing_key_back_where_the_default_file_has_it() {
    let file = File::new("update_each_key");
    let default = default_file();
    let keys: Vec<_> = blocks(&default)
        .into_iter()
        .filter(|(path, _)| !path.starts_with('['))
        .collect();
    assert_eq!(keys.len(), DEFAULT_KEY_COUNT);
    for (path, block) in keys {
        let text = default.replacen(&block, "", 1);
        assert_ne!(text, default, "{path}");
        let (update, updated) = write_and_update(&file, &text);
        assert_eq!(update.added(), std::slice::from_ref(&path));
        assert_eq!(updated, default, "{path}");
    }
}

#[test]
fn update_leaves_a_current_file_as_it_is() {
    let file = File::new("update_current");
    let (update, updated) = write_and_update(&file, &default_file());
    assert!(update.is_empty());
    assert_eq!(updated, default_file());
}

#[test]
fn update_adds_missing_tables_at_the_end() {
    let file = File::new("update_tables");
    let default = default_file();
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
    assert_eq!(update.added().len(), DEFAULT_KEY_COUNT - 1);

    let (update, updated) = write_and_update(&file, "[developer]\nraknet = false\n");
    assert!(updated.starts_with("[developer]\n"));
    assert!(updated.ends_with(&format!("\nconsole-input = true\n\n{general}")));
    assert_eq!(update.added().len(), DEFAULT_KEY_COUNT - 1);
}

#[test]
fn update_keeps_the_rest_of_the_file_byte_for_byte() {
    let file = File::new("update_keeps");
    // A byte order mark, CRLF line endings, comments after values and headers, a multi-line
    // string with a line like a header, a world table, an unknown table and no line ending at
    // the end.
    let text = "\u{feff}# my server\r\n[general]\r\nmotd = \"Lobby\" # shown in the list\r\n\
                shutdown-message = \"\"\"\r\n[bye]\r\n\"\"\"\r\n\r\n[developer] # tuning\r\n\
                raknet = false\r\n\r\n\
                [worlds.lobby]\r\nview-distance = 6\r\n[plugin]\r\nanswer = 42";
    let (update, updated) = write_and_update(&file, text);
    assert_eq!(update.added().len(), DEFAULT_KEY_COUNT - 3);
    for kept in [
        "general.motd",
        "general.shutdown-message",
        "developer.raknet",
    ] {
        assert!(!update.added().iter().any(|p| p == kept), "{kept}");
    }
    assert!(!updated.replace("\r\n", "").contains('\n'), "only CRLF");
    let default = blocks(&default_file());
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

    // A key goes after the last line of a multi-line array.
    let text = "[general]\r\nresource-packs = [\r\n    \"a.mcpack\",\r\n]\r\n\
                log-level = \"warn\"\r\n";
    let (_, updated) = write_and_update(&file, text);
    assert!(updated.contains(
        "]\r\n# true: players must accept the resource packs to join.\r\n\
         force-resources = false\r\nlog-level = \"warn\"\r\n"
    ));
}

#[test]
fn update_removes_unknown_keys_with_the_comment_lines_right_above_them() {
    let file = File::new("update_unknown");
    let default = default_file();
    // A root key before the first comment, a key right below a header, a quoted key with a
    // multi-line value and a comment after it, and a key at the end without a line ending, a
    // blank line keeping the comment lines above it.
    let text = format!("gone = 1\n{default}")
        .replacen(
            "[general]\n",
            "[general]\n# Old.\n#  Gone.\nold-key = 1\n",
            1,
        )
        .replacen(
            "[developer]\n",
            "[developer]\n  # Old.\n  \"old key\" = [\n    1,\n] # gone\n",
            1,
        )
        + "\n# Old.\nlast = true";
    write(&file.path, text.as_bytes());
    let update = Config::update_file(&file.path).expect("updates");
    assert_eq!(
        update.removed(),
        [
            "gone",
            "general.old-key",
            "developer.old key",
            "developer.last"
        ]
    );
    assert!(update.added().is_empty());
    assert_eq!(read(&file.path), format!("{default}\n"));
    assert!(Config::update_file(&file.path).expect("again").is_empty());

    // A blank line ends the comment lines of a key, and a line of a multi-line string is not a
    // comment line; CRLF lines are removed whole.
    let text = "[general]\r\nmotd = \"\"\"\r\n# Lobby\"\"\"\r\nold-key = 1\r\n# Kept.\r\n\r\n\
                # Old.\r\nother = 1\r\npvp = false\r\n";
    write(&file.path, text.as_bytes());
    let update = Config::update_file(&file.path).expect("updates");
    assert_eq!(update.removed(), ["general.old-key", "general.other"]);
    let updated = read(&file.path);
    assert!(
        updated.contains("motd = \"\"\"\r\n# Lobby\"\"\"\r\n"),
        "{updated}"
    );
    assert!(
        updated.contains("\r\n# Kept.\r\n\r\npvp = false\r\n"),
        "{updated}"
    );
    assert!(!updated.contains("# Old.") && !updated.contains("old-key"));

    // A table under a known table and a dotted key are left for the load to name.
    for (text, expected) in [
        (
            "[general]\n[general.old]\nanswer = 1\n",
            "unknown key `general.old`",
        ),
        ("[general]\nold.answer = 1\n", "unknown key `general.old`"),
        (
            "[worlds.lobby]\nold.answer = 1\n",
            "unknown key `worlds.lobby.old`",
        ),
    ] {
        write(&file.path, text.as_bytes());
        let error = Config::update_file(&file.path).expect_err(expected);
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(read(&file.path), text);
    }
}

#[test]
fn update_removes_unknown_keys_of_world_tables_and_keeps_their_values() {
    let file = File::new("update_world_keys");
    let text = "[worlds.lobby]\n# Old.\nold-key = 1\nview-distance = 6\n\
                [worlds.\"my.world\"]\nspawn = [0, 64, 0]\nstale = true\n";
    write(&file.path, text.as_bytes());
    let update = Config::update_file(&file.path).expect("updates");
    assert_eq!(
        update.removed(),
        ["worlds.lobby.old-key", "worlds.my.world.stale"]
    );
    let updated = read(&file.path);
    assert!(
        updated.starts_with(
            "[worlds.lobby]\nview-distance = 6\n[worlds.\"my.world\"]\nspawn = [0, 64, 0]\n"
        ),
        "{updated}"
    );
    assert!(!updated.contains("# Old.") && !updated.contains("stale"));
    let loaded = Config::default().load(&file.path).expect("loads");
    assert_eq!(loaded.worlds["lobby"].view_distance, Some(6));
    assert_eq!(
        loaded.worlds["my.world"].spawn,
        Some(BlockPos::new(0, 64, 0))
    );
    assert!(Config::update_file(&file.path).expect("again").is_empty());
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
