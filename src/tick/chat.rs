//! Chat: the `Text` packets players send, checked and broadcast.
//!
//! A chat packet is checked first ([`check`]: the sender, the per-tick limit, the length, the
//! cleaning), then applied ([`apply`]: the limit is spent, a line starting with `/` runs as the
//! player's command, and each other line asks [`PlayerChat`] and goes to its recipients).

use tracing::{debug, trace};

use super::interact::sign::clean;
use super::queue::SessionId;
use super::{Context, Phase, Player, State, dispatch, inventory, player};
use crate::command::{Line, Sender};
use crate::event::PlayerChat;
use crate::player::{Id, MAX_TEXT, Message, Packets};
use crate::protocol::consts::TextType;
use crate::protocol::packet::Text;

/// The translation key of a chat line, `<name> message`, for a
/// speaker whose name does not fit a chat source ([`standard`]).
const CHAT_FORMAT: &str = "chat.type.text";
/// Chat lines and command requests a player may send in one tick, together (a command request
/// spends a chat line).
const LINES_PER_TICK: u8 = 2;
/// The longest chat line the official server relays, in bytes (P6 `limits`, `xtask/data/p6/facts.md`).
const MAX_CHAT_BYTES: usize = 512;

/// The lines a chat message carries, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lines {
    /// The lines, at most [`LINES_PER_TICK`].
    lines: Vec<Part>,
    /// The lines of the per-tick limit spent, this packet's included.
    spent: u8,
}

/// One line of a chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// A chat line.
    Chat(String),
    /// A command line, without its `/`.
    Command(String),
}

impl Context<'_> {
    /// Says `message` in chat as `player` would have typed it ([`crate::action::Chat`]): the same
    /// limits as the client's chat, its commands and its [`PlayerChat`] events.
    pub(crate) fn chat_as(&mut self, player: Id, message: &str) {
        chat(self.state, player.session(), message);
    }
}

/// Handles a `Text` packet of `session`: a chat message of a spawned player. Every other text
/// type, and the sender name and XUID the packet carries, are ignored: the server knows who
/// speaks.
pub(super) fn on_text(state: &mut State, session: SessionId, packet: &Text) {
    if packet.text_type != u8::from(TextType::Chat) {
        trace!(
            session = session.get(),
            text_type = packet.text_type,
            "non-chat text ignored"
        );
        return;
    }
    chat(state, session, &packet.message);
}

/// Says `message` for `session`: only a spawned player speaks, and
/// it closes its open window first; then the message is checked and applied.
fn chat(state: &mut State, session: SessionId, message: &str) {
    if !state
        .players
        .get(&session)
        .is_some_and(|p| p.phase == Phase::Spawned)
    {
        return;
    }
    inventory::close_window(state, session);
    if let Some(lines) = check(state, session, message) {
        apply(state, session, lines);
    }
}

/// Spends one line of the per-tick limit of the player of `session` for a command request;
/// false, spending nothing, if none is left (a command request is a chat line, so the
/// two share the limit).
pub(super) fn spend_line(state: &mut State, session: SessionId) -> bool {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return false;
    };
    let left = lines_left(player, tick);
    if left == 0 {
        return false;
    }
    player.marks.chat = (tick, LINES_PER_TICK - left + 1);
    true
}

/// The lines `message` holds: nothing once the player sent 2 lines this
/// tick or for a message longer than the lines it may still send; then the message is cleaned
/// (private-use characters and terminal escapes removed, `§` codes kept) and split at every line
/// break. Blank lines and lines over 512 bytes (the official server's limit, P6 `limits`)
/// are dropped, and the first lines that remain are kept up to the
/// limit left, each spending one of it. A line that starts with `/` (or `./`, whose dot is
/// dropped) is a command.
fn check(state: &State, session: SessionId, message: &str) -> Option<Lines> {
    let player = state.players.get(&session)?;
    let mut left = lines_left(player, state.tick);
    if left == 0 || message.len() > usize::from(left) * (MAX_CHAT_BYTES + 1) {
        return None;
    }
    let cleaned = clean(message);
    let mut lines = Vec::with_capacity(usize::from(left));
    for part in cleaned.split('\n') {
        if left == 0 {
            break;
        }
        if part.len() > MAX_CHAT_BYTES || trim(part).is_empty() {
            continue;
        }
        left -= 1;
        let part = part
            .strip_prefix('.')
            .filter(|p| p.starts_with('/'))
            .unwrap_or(part);
        lines.push(match part.strip_prefix('/') {
            Some(command) => Part::Command(command.to_owned()),
            None => Part::Chat(part.to_owned()),
        });
    }
    Some(Lines {
        lines,
        spent: LINES_PER_TICK - left,
    })
}

/// The lines of the per-tick limit `player` has left at engine tick `tick`.
fn lines_left(player: &Player, tick: u64) -> u8 {
    let (at, used) = player.marks.chat;
    if at == tick {
        LINES_PER_TICK.saturating_sub(used)
    } else {
        LINES_PER_TICK
    }
}

/// Spends the player's limit and does its lines in order: a command runs as the player's
/// and its output goes to the player in chat; a chat line goes to
/// every online player as `<name> line` with the speaker's display name, unless a [`PlayerChat`] listener changes it.
fn apply(state: &mut State, session: SessionId, lines: Lines) {
    let tick = state.tick;
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    player.marks.chat = (tick, lines.spent);
    for part in lines.lines {
        match part {
            Part::Command(line) => command(state, session, &line),
            Part::Chat(line) => say(state, session, line),
        }
    }
}

/// Runs `line` as the command of the player of `session` and shows it the output as a
/// `CommandRequest` would ([`shown`](super::command::shown)): each line in chat, an error in red.
fn command(state: &mut State, session: SessionId, line: &str) {
    let Some(result) = super::command::typed(state, Sender::Player(Id::new(session)), line) else {
        debug!(session = session.get(), "chat command cancelled");
        return;
    };
    debug!(session = session.get(), ok = result.is_ok(), "chat command");
    let (success, lines) = super::command::shown(&result);
    let Some(player) = state.players.get_mut(&session) else {
        return;
    };
    for Line { text, params } in lines {
        let message = if params.is_empty() {
            Message::Raw(if success { text } else { format!("§c{text}") })
        } else {
            Message::Translation {
                key: text,
                parameters: params,
            }
        };
        if let Err(e) = player.send(message) {
            debug!(
                session = session.get(),
                error = &e as &(dyn std::error::Error + 'static),
                "command output not sent"
            );
        }
    }
}

/// Sends one chat line of the player of `session`, if it is online. Without [`PlayerChat`]
/// listeners it goes to every online player in the standard format. A listener may change the
/// speaker, the message, the recipients and the format; what it leaves is checked again: the
/// speaker must still be online, and a recipient that left or is listed twice is skipped. A line
/// a client would not decode (over 65,536 bytes) is not sent, and no monitor sees it, as with a
/// cancel.
fn say(state: &mut State, session: SessionId, line: String) {
    if !dispatch::heard::<PlayerChat>(state) {
        let Some(name) = display_name(state, Id::new(session)) else {
            return;
        };
        debug!(session = session.get(), %name, message = %line, "chat");
        match standard(&name, &line) {
            Ok(packets) => {
                player::broadcast_packets(state, &packets);
            }
            Err(e) => debug!(
                session = session.get(),
                error = &e as &(dyn std::error::Error + 'static),
                "chat line not sent"
            ),
        }
        return;
    }
    if online(state, Id::new(session)).is_none() {
        return;
    }
    let recipients = state
        .players
        .values()
        .filter(|p| p.gets_broadcasts())
        .map(Player::id)
        .collect();
    let mut event = PlayerChat {
        player: Id::new(session),
        message: line,
        recipients,
        format: None,
        cancelled: false,
    };
    dispatch::call(state, &mut event);
    if event.cancelled {
        return;
    }
    let Some(name) = display_name(state, event.player) else {
        return;
    };
    event.recipients.sort_unstable();
    event.recipients.dedup();
    let packets = match &event.format {
        Some(format) => {
            let Some(text) = formatted(format, &name, &event.message) else {
                debug!(
                    session = session.get(),
                    "formatted chat line too long; not sent"
                );
                return;
            };
            Message::Raw(text).into_packets()
        }
        None => standard(&name, &event.message),
    };
    let packets = match packets {
        Ok(packets) => packets,
        Err(e) => {
            debug!(
                session = session.get(),
                error = &e as &(dyn std::error::Error + 'static),
                "chat line not sent"
            );
            return;
        }
    };
    debug!(session = session.get(), %name, message = %event.message, "chat");
    for &id in &event.recipients {
        if let Some(p) = state
            .players
            .get_mut(&id.session())
            .filter(|p| p.gets_broadcasts())
        {
            for packet in packets.iter().flatten() {
                p.send_packet(packet.clone());
            }
        }
    }
    dispatch::watch(state, &event);
}

/// The packets of `name` saying `line` in the standard format, as the official server sends them: a chat `Text`
/// from `<name>§r` with the line as it is, untranslated (the client shows `<name> line`). A
/// display name too long for a chat source keeps the translated form ([`CHAT_FORMAT`]), which
/// carries the name as a parameter, so the line is still sent.
fn standard(name: &str, line: &str) -> crate::player::Result<Packets> {
    Message::Chat {
        source: format!("{name}§r"),
        text: line.to_owned(),
    }
    .into_packets()
    .or_else(|_| {
        Message::Translation {
            key: CHAT_FORMAT.to_owned(),
            parameters: vec![name.to_owned(), line.to_owned()],
        }
        .into_packets()
    })
}

/// The display name of the player `id` if it is online.
fn display_name(state: &State, id: Id) -> Option<String> {
    online(state, id).map(|p| p.display_name().to_owned())
}

/// The player `id` if it is online: joined and not leaving.
fn online(state: &State, id: Id) -> Option<&Player> {
    state
        .players
        .get(&id.session())
        .filter(|p| p.gets_broadcasts())
}

/// `format` with `{%0}` replaced by `name`, `{%1}` by `message` and `{%%` by `{%` in one pass
/// (a replacement is not rescanned), or `None` if the line would be longer
/// than a client decodes.
fn formatted(format: &str, name: &str, message: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = format;
    while let Some(at) = rest.find("{%") {
        let (before, tail) = rest.split_at(at);
        let (value, skip) = if tail.starts_with("{%%") {
            ("{%", 3)
        } else {
            match tail.get(..4) {
                Some("{%0}") => (name, 4),
                Some("{%1}") => (message, 4),
                _ => ("{%", 2),
            }
        };
        push_bounded(&mut out, before)?;
        push_bounded(&mut out, value)?;
        rest = tail.get(skip..).unwrap_or_default();
    }
    push_bounded(&mut out, rest)?;
    Some(out)
}

/// Appends `text` to `out` unless `out` would pass [`MAX_TEXT`] bytes.
fn push_bounded(out: &mut String, text: &str) -> Option<()> {
    if out.len().checked_add(text.len())? > MAX_TEXT {
        return None;
    }
    out.push_str(text);
    Some(())
}

/// `text` without the white space PHP's `trim` removes.
fn trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\0', '\u{b}'])
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;

    use crate::event::Listeners;
    use crate::protocol::packet::{CommandRequest, Packet};
    use crate::tick::Output;
    use crate::tick::queue::{Input, Outbound};
    use crate::tick::tests::{
        initialised, input, join, new_state, packet, radius, received, settings, tick,
    };

    /// Players 1 and 2 spawned, and their outbound queues emptied.
    fn server() -> (State, [mpsc::Receiver<Outbound>; 2]) {
        server_with(Listeners::default())
    }

    /// [`server`] with `listeners`.
    fn server_with(listeners: Listeners) -> (State, [mpsc::Receiver<Outbound>; 2]) {
        let mut state = new_state(settings(|_| {}), listeners);
        let mut output = Output::default();
        let (j1, mut rx1) = join(1, 0);
        let (j2, mut rx2) = join(2, 0);
        tick(
            &mut state,
            &mut vec![j1, j2, radius(1, 1, 4), radius(2, 1, 4)],
            &mut output,
        );
        let ids: Vec<u64> = [1, 2]
            .map(|s| state.players[&SessionId::new(s)].runtime_id)
            .into();
        tick(
            &mut state,
            &mut vec![initialised(1, 2, ids[0]), initialised(2, 2, ids[1])],
            &mut output,
        );
        received(&mut rx1);
        received(&mut rx2);
        (state, [rx1, rx2])
    }

    fn chat(session: u64, seq: u64, message: &str) -> Input {
        input(
            session,
            seq,
            packet(Packet::Text(Box::new(Text {
                text_type: TextType::Chat.into(),
                source_name: "someone else".to_owned(),
                message: message.to_owned(),
                xuid: "123".to_owned(),
                ..Text::default()
            }))),
        )
    }

    /// The chat lines a session received, `[speaker, line]`: the chat `Text` from `<name>§r`,
    /// untranslated and without parameters.
    fn lines(rx: &mut mpsc::Receiver<Outbound>) -> Vec<Vec<String>> {
        received(rx)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::Text(t) if t.text_type == u8::from(TextType::Chat) => {
                    assert!(!t.needs_translation);
                    assert!(t.parameters.is_empty());
                    let name = t
                        .source_name
                        .strip_suffix("§r")
                        .expect("the source name ends with a reset code");
                    Some(vec![name.to_owned(), t.message])
                }
                _ => None,
            })
            .collect()
    }

    fn run(state: &mut State, inputs: Vec<Input>) {
        tick(state, &mut { inputs }, &mut Output::default());
    }

    #[test]
    fn chat_reaches_every_player_with_the_server_known_name() {
        let (mut state, [mut rx1, mut rx2]) = server();
        run(&mut state, vec![chat(1, 10, "hello")]);
        let expected = vec![vec!["player1".to_owned(), "hello".to_owned()]];
        assert_eq!(lines(&mut rx1), expected);
        assert_eq!(lines(&mut rx2), expected);
    }

    #[test]
    fn chat_has_the_official_shape_and_a_name_too_long_for_a_source_is_translated() {
        let (mut state, [_rx1, mut rx2]) = server();
        run(&mut state, vec![chat(1, 10, "hello")]);
        let got: Vec<Box<Text>> = received(&mut rx2)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (
                got[0].text_type,
                got[0].source_name.as_str(),
                got[0].message.as_str(),
                got[0].needs_translation
            ),
            (u8::from(TextType::Chat), "player1§r", "hello", false)
        );
        let long = "n".repeat(300);
        state
            .players
            .get_mut(&SessionId::new(1))
            .unwrap()
            .set_display_name(long.clone())
            .unwrap();
        run(&mut state, vec![chat(1, 11, "hi")]);
        let got: Vec<(String, Vec<String>)> = received(&mut rx2)
            .0
            .into_iter()
            .filter_map(|p| match p {
                Packet::Text(t) if t.text_type == u8::from(TextType::Translation) => {
                    Some((t.message, t.parameters))
                }
                _ => None,
            })
            .collect();
        assert_eq!(got, [(CHAT_FORMAT.to_owned(), vec![long, "hi".to_owned()])]);
    }

    #[test]
    fn the_display_name_speaks_and_a_repeated_packet_is_a_second_message() {
        let (mut state, [_rx1, mut rx2]) = server();
        state
            .players
            .get_mut(&SessionId::new(1))
            .unwrap()
            .set_display_name("Boss")
            .unwrap();
        run(&mut state, vec![chat(1, 10, "hi")]);
        run(&mut state, vec![chat(1, 10, "hi")]);
        let got = lines(&mut rx2);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|l| l[0] == "Boss" && l[1] == "hi"));
    }

    #[test]
    fn two_lines_a_tick_pass_and_the_rest_wait_for_the_next_tick() {
        let (mut state, [_rx1, mut rx2]) = server();
        run(
            &mut state,
            vec![chat(1, 10, "a\nb\nc"), chat(1, 11, "d"), chat(1, 12, "e")],
        );
        let got: Vec<String> = lines(&mut rx2).into_iter().map(|l| l[1].clone()).collect();
        // The first packet's two lines spend the limit: its third line and the tick's next
        // packets are dropped.
        assert_eq!(got, ["a", "b"]);
        run(&mut state, vec![chat(1, 13, "f")]);
        assert_eq!(lines(&mut rx2), [["player1".to_owned(), "f".to_owned()]]);
    }

    #[test]
    fn long_blank_command_and_non_chat_lines_are_not_broadcast() {
        let (mut state, [_rx1, mut rx2]) = server();
        // The official server relays a chat line of at most 512 bytes (P6 `limits`): 170 three-byte characters
        // pass, 171 do not.
        let long = "x".repeat(MAX_CHAT_BYTES + 1);
        let wide = "가".repeat(170);
        let too_wide = "가".repeat(171);
        let other = Text {
            text_type: TextType::Raw.into(),
            message: "raw".to_owned(),
            ..Text::default()
        };
        run(
            &mut state,
            vec![
                chat(1, 10, &long),
                chat(1, 11, "  \t "),
                chat(1, 12, &too_wide),
                chat(1, 13, "/help"),
                input(1, 14, packet(Packet::Text(Box::new(other)))),
            ],
        );
        assert!(lines(&mut rx2).is_empty());
        // 510 bytes fit; `./me` is a command, as `/me` is.
        run(&mut state, vec![chat(1, 15, &wide), chat(1, 16, "./me")]);
        let got: Vec<String> = lines(&mut rx2).into_iter().map(|l| l[1].clone()).collect();
        assert_eq!(got, [wide]);
        // A player that has not spawned cannot chat.
        let (j, mut rx3) = join(3, 0);
        run(&mut state, vec![j, chat(3, 1, "early")]);
        assert!(lines(&mut rx2).is_empty());
        assert!(lines(&mut rx3).is_empty());
    }

    #[test]
    fn cleaning_removes_private_use_characters_and_terminal_escapes_but_keeps_format_codes() {
        let (mut state, [_rx1, mut rx2]) = server();
        let cases = [
            ("§ahi\u{e000}!\u{f8ff}", "§ahi!"),
            ("a\u{1b}[31mred\u{1b}(B", "ared"),
            ("\u{1b}[1;2x", "[1;2x"),
            ("a\u{1b}\u{e000}[31mb", "ab"),
        ];
        for (seq, (said, shown)) in (10..).zip(cases) {
            run(&mut state, vec![chat(1, seq, said)]);
            assert_eq!(lines(&mut rx2), [["player1".to_owned(), shown.to_owned()]]);
        }
    }

    #[test]
    fn trimming_removes_the_white_space_php_removes() {
        assert_eq!(trim("\0 x \u{b}"), "x");
    }

    #[test]
    fn every_line_break_ends_a_line_so_no_line_carries_one() {
        let (mut state, [_rx1, mut rx2]) = server();
        run(&mut state, vec![chat(1, 10, "\n\nhi\n§e<Admin> x")]);
        assert_eq!(
            lines(&mut rx2),
            [
                ["player1".to_owned(), "hi".to_owned()],
                ["player1".to_owned(), "§e<Admin> x".to_owned()],
            ]
        );
    }

    #[test]
    fn a_format_writes_a_literal_brace_percent_as_brace_percent_percent() {
        assert_eq!(
            formatted("[{%%0}] {%0}: {%1} {%%}{%2 {%", "a{%1}", "b").as_deref(),
            Some("[{%0}] a{%1}: b {%}{%2 {%")
        );
    }

    fn request(session: u64, seq: u64, line: &str) -> Input {
        input(
            session,
            seq,
            packet(Packet::CommandRequest(CommandRequest {
                command_line: line.to_owned(),
                ..CommandRequest::default()
            })),
        )
    }

    /// How many `CommandOutput` packets a session received.
    fn outputs(rx: &mut mpsc::Receiver<Outbound>) -> usize {
        received(rx)
            .0
            .iter()
            .filter(|p| matches!(p, Packet::CommandOutput(_)))
            .count()
    }

    #[test]
    fn chat_lines_and_command_requests_share_two_a_tick() {
        let (mut state, [mut rx1, mut rx2]) = server();
        // Two requests spend the tick's limit: the chat line after them is dropped.
        run(
            &mut state,
            vec![
                request(1, 10, "/nope"),
                request(1, 11, "/nope"),
                chat(1, 12, "hi"),
                request(1, 13, "/nope"),
            ],
        );
        assert_eq!(outputs(&mut rx1), 2);
        assert!(lines(&mut rx2).is_empty());
        // A chat line and a request pass; the second request waits for the next tick.
        run(
            &mut state,
            vec![
                chat(1, 14, "a"),
                request(1, 15, "/nope"),
                request(1, 16, "/nope"),
            ],
        );
        assert_eq!(outputs(&mut rx1), 1);
        assert_eq!(lines(&mut rx2).len(), 1);
        run(
            &mut state,
            vec![chat(1, 17, "b\nc"), request(1, 18, "/nope")],
        );
        assert_eq!(outputs(&mut rx1), 0, "two chat lines spent the tick");
        assert_eq!(lines(&mut rx2).len(), 2);
    }

    #[test]
    fn a_message_longer_than_the_lines_left_is_dropped_whole() {
        let (mut state, [_rx1, mut rx2]) = server();
        let message = "y".repeat(2 * (MAX_CHAT_BYTES + 1) + 1);
        run(&mut state, vec![chat(1, 10, &message)]);
        assert!(lines(&mut rx2).is_empty());
        assert_eq!(state.players[&SessionId::new(1)].marks.chat.1, 0);
    }

    #[test]
    fn a_kicked_speaker_says_nothing_with_or_without_chat_listeners() {
        for idle in [false, true] {
            let mut listeners = Listeners::default();
            if idle {
                listeners.push(
                    crate::event::Priority::Normal,
                    |_: &mut PlayerChat, _: &mut Context<'_>| {},
                );
            }
            let (mut state, [_rx1, mut rx2]) = server_with(listeners);
            state
                .players
                .get_mut(&SessionId::new(1))
                .unwrap()
                .kick("bye")
                .unwrap();
            run(&mut state, vec![chat(1, 10, "hi")]);
            assert!(lines(&mut rx2).is_empty(), "idle listener: {idle}");
        }
    }

    #[test]
    fn a_line_no_client_decodes_is_sent_to_nobody_and_no_monitor_sees_it() {
        let (seen, mut monitored) = mpsc::channel(4);
        let mut listeners = Listeners::default();
        listeners.push(
            crate::event::Priority::Normal,
            |e: &mut PlayerChat, _: &mut Context<'_>| e.set_message("x".repeat(MAX_TEXT + 1)),
        );
        listeners.push_monitor(move |_: &PlayerChat, _: &Context<'_>| {
            seen.try_send(()).expect("room");
        });
        let (mut state, [_rx1, mut rx2]) = server_with(listeners);
        run(&mut state, vec![chat(1, 10, "hi")]);
        assert!(lines(&mut rx2).is_empty());
        assert!(monitored.try_recv().is_err(), "the monitor saw the line");
    }
}
