//! The settings file through the public API (PLAN.md 10.2 "M2 판정식", 설정): the default file,
//! every key's defaults, types and ranges, unknown keys, and file handling.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

use std::error::Error as _;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use anchor::Config;
use anchor::config::{Difficulty, Error, GameMode, LevelType, Threads};
use anchor::protocol::compression::Compression;

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

/// Returns the toml block of PLAN.md 8.5: the canonical default file.
fn plan_default_file() -> String {
    let plan = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/PLAN.md"));
    let (_, section) = plan
        .split_once("\n### 8.5 ")
        .expect("PLAN.md has section 8.5");
    let (_, block) = section
        .split_once("```toml\n")
        .expect("8.5 has a toml block");
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
level-name = "lobby"
level-type = "void"
level-seed = "-42"
view-distance = 8
save-player-data = false
shutdown-message = "Bye"
resource-packs = ["a.mcpack", "packs/b.zip"]
force-resources = true

[developer]
compression = "zlib"
compression-threshold = 512
compression-level = 9
max-batch-bytes = 1048576
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
lighting = false
save-interval-minutes = 5
console-input = false
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
    g.level_name = "lobby".to_owned();
    g.level_type = LevelType::Void;
    g.level_seed = Some(-42);
    g.view_distance = 8;
    g.save_player_data = false;
    g.shutdown_message = "Bye".to_owned();
    g.resource_packs = vec!["a.mcpack".into(), "packs/b.zip".into()];
    g.force_resources = true;
    let d = &mut config.developer;
    d.compression = Compression::Zlib;
    d.compression_threshold = 512;
    d.compression_level = 9;
    d.max_batch_bytes = MIB;
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
    d.lighting = false;
    d.save_interval_minutes = 5;
    d.console_input = false;
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
        ("developer.lighting", bools(), vec![]),
        (
            "developer.save-interval-minutes",
            s(&["1", "1440"]),
            s(&["0", "1441"]),
        ),
        ("developer.console-input", bools(), vec![]),
    ]
}

#[test]
fn missing_file_is_created_with_the_plan_default_text() {
    let file = File::new("missing_file");
    let first = Config::default().load(&file.path).expect("loads");
    assert_eq!(first, Config::default());
    assert_eq!(read(&file.path), plan_default_file());
    assert!(!exists(&file.path.with_extension("toml.tmp")));
    let again = Config::default().load(&file.path).expect("loads again");
    assert_eq!(again, first);
}

#[test]
fn missing_file_reads_like_the_file_it_writes() {
    let file = File::new("missing_file_over_code");
    let first = custom().load(&file.path).expect("loads");
    assert_eq!(first, Config::default());
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
    let default = plan_default_file();
    let keys = key_lines(&default);
    assert_eq!(keys.len(), 42, "PLAN.md 8.5 has 42 keys");
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
    for (table, key, value) in key_lines(&plan_default_file()) {
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

#[test]
fn every_key_accepts_its_edges_and_rejects_beyond() {
    let file = File::new("every_key_edges");
    let cases = cases();
    let mut listed: Vec<String> = cases.iter().map(|(k, _, _)| (*k).to_owned()).collect();
    let mut keys: Vec<String> = key_lines(&plan_default_file())
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
