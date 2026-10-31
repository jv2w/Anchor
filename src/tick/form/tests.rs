//! Forms, scoreboards and boss bars through the tick: the packets sent, the answers accepted and
//! dropped, and the tables left behind.

use tokio::sync::mpsc;

use super::*;
use crate::display::{BossBar, Colour, Error as DisplayError, Scoreboard, Slot, Sort};
use crate::event::Listeners;
use crate::form::{Custom, Menu, Modal, Value};
use crate::protocol::consts::{BossEventType, ScoreboardIdentity};
use crate::tick::Output;
use crate::tick::queue::{Input, Outbound};
use crate::tick::tests::{input, join, leave, new_state, packet, received, settings, tick};

/// Each handler call: the session and the response.
type Calls = mpsc::Receiver<(u64, Response)>;

/// A server with players 1 and 2 joined, and their outbound queues.
fn server() -> (State, Vec<mpsc::Receiver<Outbound>>) {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    let (a, rx1) = join(1, 0);
    let (b, rx2) = join(2, 0);
    tick(&mut state, &mut vec![a, b], &mut Output::default());
    (state, vec![rx1, rx2])
}

fn id(session: u64) -> player::Id {
    player::Id::new(SessionId::new(session))
}

/// A handler that records its calls, and the record.
fn recorder() -> (
    impl FnOnce(&mut Context<'_>, player::Id, Response) + Send + 'static,
    Calls,
) {
    let (tx, rx) = mpsc::channel(64);
    let handler = move |_: &mut Context<'_>, p: player::Id, r: Response| {
        tx.try_send((p.session().get(), r)).expect("room");
    };
    (handler, rx)
}

fn send(state: &mut State, session: u64, form: impl Into<Form>) -> Calls {
    let (handler, calls) = recorder();
    Context { state }
        .send_form(id(session), form, handler)
        .expect("sent");
    calls
}

fn calls(calls: &mut Calls) -> Vec<(u64, Response)> {
    let mut out = Vec::new();
    while let Ok(c) = calls.try_recv() {
        out.push(c);
    }
    out
}

fn answer(session: u64, seq: u64, form_id: u32, data: Option<&[u8]>, cancel: Option<u8>) -> Input {
    let p = ModalFormResponse {
        form_id,
        response_data: data.map(<[u8]>::to_vec),
        cancel_reason: cancel,
    };
    input(session, seq, packet(Packet::ModalFormResponse(p)))
}

/// Runs one tick of `inputs`.
fn run(state: &mut State, inputs: Vec<Input>) {
    tick(state, &mut { inputs }, &mut Output::default());
}

/// The form, scoreboard and boss bar packets queued for a player since the last call.
fn packets(rx: &mut mpsc::Receiver<Outbound>) -> Vec<Packet> {
    received(rx)
        .0
        .into_iter()
        .filter(|p| {
            matches!(
                p,
                Packet::ModalFormRequest(_)
                    | Packet::ClientBoundCloseForm(_)
                    | Packet::SetDisplayObjective(_)
                    | Packet::SetScore(_)
                    | Packet::RemoveObjective(_)
                    | Packet::BossEvent(_)
            )
        })
        .collect()
}

fn requests(rx: &mut mpsc::Receiver<Outbound>) -> Vec<ModalFormRequest> {
    packets(rx)
        .into_iter()
        .filter_map(|p| match p {
            Packet::ModalFormRequest(r) => Some(r),
            _ => None,
        })
        .collect()
}

fn pending(state: &State, session: u64) -> Vec<u32> {
    state
        .forms
        .players
        .get(&SessionId::new(session))
        .map(|o| o.pending.keys().copied().collect())
        .unwrap_or_default()
}

fn menu() -> Menu {
    Menu::new("Lobby").content("Pick").button("a").button("b")
}

// pmmp: Player::sendForm
#[test]
fn a_sent_form_is_a_modal_form_request_with_its_json_and_ids_increase() {
    let (mut state, mut rx) = server();
    let _a = send(&mut state, 1, menu());
    let _b = send(&mut state, 1, Modal::new("t", "c", "y", "n"));
    let _c = send(&mut state, 2, menu());
    run(&mut state, vec![]);
    let sent = requests(&mut rx[0]);
    assert_eq!(
        sent.iter().map(|r| r.form_id).collect::<Vec<_>>(),
        [0, 1],
        "per player, from 0"
    );
    let (json, _) = Form::from(menu()).prepare().unwrap();
    assert_eq!(sent[0].form_data, json);
    let value: serde_json::Value = serde_json::from_slice(&sent[1].form_data).unwrap();
    assert_eq!(value["type"], "modal");
    assert_eq!(requests(&mut rx[1])[0].form_id, 0);
    assert_eq!(pending(&state, 1), [0, 1]);
}

// pmmp: Form::handleResponse
#[test]
fn a_valid_answer_calls_the_handler_once_with_the_typed_response() {
    let (mut state, _rx) = server();
    let mut menu_calls = send(&mut state, 1, menu());
    let custom = Custom::new("C")
        .label("l")
        .input("name", "", "")
        .toggle("t", false);
    let mut custom_calls = send(&mut state, 1, custom);
    let mut modal_calls = send(&mut state, 2, Modal::new("t", "c", "y", "n"));
    run(
        &mut state,
        vec![
            answer(1, 1, 0, Some(b"1"), None),
            answer(1, 2, 1, Some(br#"[null,"Alex",true]"#), None),
            answer(2, 1, 0, Some(b"false"), None),
        ],
    );
    assert_eq!(calls(&mut menu_calls), [(1, Response::Menu(1))]);
    assert_eq!(
        calls(&mut custom_calls),
        [(
            1,
            Response::Custom(vec![
                Value::Empty,
                Value::Text("Alex".into()),
                Value::Bool(true)
            ])
        )]
    );
    assert_eq!(calls(&mut modal_calls), [(2, Response::Modal(false))]);
    assert!(pending(&state, 1).is_empty() && pending(&state, 2).is_empty());
    run(&mut state, vec![answer(1, 3, 0, Some(b"0"), None)]);
    assert!(
        calls(&mut menu_calls).is_empty(),
        "a second answer is ignored"
    );
}

#[test]
fn the_same_answer_twice_changes_nothing_twice() {
    let run_with = |answers: u64| {
        let (mut state, _rx) = server();
        let mut c = send(&mut state, 1, menu());
        let inputs = (1..=answers)
            .map(|seq| answer(1, seq, 0, Some(b"0"), None))
            .collect();
        run(&mut state, inputs);
        let mut h = crate::tick::Fnv::new();
        state.forms.hash_into(&mut h);
        (state.hash(), h.0, calls(&mut c).len())
    };
    let once = run_with(1);
    assert_eq!(once.2, 1);
    assert_eq!(run_with(2), once);
}

#[test]
fn answers_that_do_not_fit_are_dropped_and_the_handler_never_runs() {
    let slider = || {
        Custom::new("C")
            .slider("s", 0.0, 10.0, 1.0, 5.0)
            .dropdown("d", ["a"], 0)
    };
    let oversize = format!("[{}0, 0]", " ".repeat(form::MAX_RESPONSE_BYTES));
    let cases: Vec<(Form, Option<&[u8]>)> = vec![
        (menu().into(), Some(b"{")),
        (menu().into(), Some(b"2")),
        (menu().into(), Some(b"true")),
        (menu().into(), None),
        (Modal::new("", "", "", "").into(), Some(b"0")),
        (slider().into(), Some(b"[11, 0]")),
        (slider().into(), Some(b"[2.5, 0]")),
        (slider().into(), Some(b"[2, 1]")),
        (slider().into(), Some(b"[true, 0]")),
        (slider().into(), Some(b"[2]")),
        (slider().into(), Some(oversize.as_bytes())),
    ];
    let (mut state, _rx) = server();
    for (seq, (form, data)) in (1..).zip(cases) {
        let (handler, mut c) = recorder();
        Context { state: &mut state }
            .send_form(id(1), form, handler)
            .unwrap();
        let form_id = *pending(&state, 1).last().unwrap();
        run(&mut state, vec![answer(1, seq, form_id, data, None)]);
        assert!(calls(&mut c).is_empty(), "case {seq}");
        assert!(
            pending(&state, 1).is_empty(),
            "case {seq}: the answer used the ID"
        );
    }
    let mut c = send(&mut state, 1, menu());
    run(&mut state, vec![answer(1, 100, 999, Some(b"0"), None)]);
    run(&mut state, vec![answer(2, 100, 11, Some(b"0"), None)]);
    assert!(calls(&mut c).is_empty());
    assert_eq!(
        pending(&state, 1),
        [11],
        "an unknown ID leaves the form waiting"
    );
}

#[test]
fn a_closed_form_calls_the_handler_with_closed() {
    let (mut state, _rx) = server();
    let mut a = send(&mut state, 1, menu());
    let mut b = send(&mut state, 1, Custom::new("C").toggle("t", true));
    let mut c = send(&mut state, 1, Modal::new("", "", "", ""));
    run(
        &mut state,
        vec![
            answer(1, 1, 0, None, Some(0)),
            answer(1, 2, 1, Some(b"[true]"), Some(1)),
            answer(1, 3, 2, Some(b"null"), None),
        ],
    );
    for calls_of in [&mut a, &mut b, &mut c] {
        assert_eq!(calls(calls_of), [(1, Response::Closed)]);
    }
}

#[test]
fn leaving_drops_the_pending_forms_without_calling_their_handlers() {
    let (mut state, _rx) = server();
    let mut c = send(&mut state, 1, menu());
    run(&mut state, vec![leave(1, 1)]);
    assert!(!state.forms.players.contains_key(&SessionId::new(1)));
    assert!(calls(&mut c).is_empty());
    let (handler, _) = recorder();
    let mut cx = Context { state: &mut state };
    assert_eq!(cx.send_form(id(1), menu(), handler), Err(Error::Offline));
    let (handler, _) = recorder();
    assert_eq!(cx.send_form(id(9), menu(), handler), Err(Error::Offline));
}

#[test]
fn at_most_16_forms_wait_and_ids_never_wrap() {
    let (mut state, _rx) = server();
    let _calls: Vec<Calls> = (0..16).map(|_| send(&mut state, 1, menu())).collect();
    let (handler, _) = recorder();
    let full = Context { state: &mut state }.send_form(id(1), menu(), handler);
    assert_eq!(full, Err(Error::Full));
    run(&mut state, vec![answer(1, 1, 3, Some(b"0"), None)]);
    let _again = send(&mut state, 1, menu());
    assert_eq!(*pending(&state, 1).last().unwrap(), 16);

    assert!(
        !state.forms.players.contains_key(&SessionId::new(2)),
        "no entry until a player is sent something"
    );
    let _first = send(&mut state, 2, menu());
    state
        .forms
        .players
        .get_mut(&SessionId::new(2))
        .unwrap()
        .next = u64::from(u32::MAX);
    let _last = send(&mut state, 2, menu());
    assert_eq!(pending(&state, 2), [0, u32::MAX]);
    let (handler, _) = recorder();
    let exhausted = Context { state: &mut state }.send_form(id(2), menu(), handler);
    assert_eq!(exhausted, Err(Error::Exhausted));
}

#[test]
fn an_invalid_form_is_not_sent() {
    let (mut state, mut rx) = server();
    let (handler, _) = recorder();
    let bad = Custom::new("C").dropdown("d", Vec::<String>::new(), 0);
    let sent = Context { state: &mut state }.send_form(id(1), bad, handler);
    assert!(matches!(sent, Err(Error::Invalid(_))));
    run(&mut state, vec![]);
    assert!(requests(&mut rx[0]).is_empty());
    assert!(pending(&state, 1).is_empty());
}

#[test]
fn a_handler_may_send_the_next_form() {
    let (mut state, mut rx) = server();
    let (tx, mut c) = mpsc::channel(4);
    Context { state: &mut state }
        .send_form(id(1), menu(), move |cx, player, _| {
            let next = move |_: &mut Context<'_>, _: player::Id, r: Response| {
                tx.try_send(r).expect("room");
            };
            cx.send_form(player, Modal::new("", "", "", ""), next)
                .expect("sent");
        })
        .unwrap();
    run(&mut state, vec![answer(1, 1, 0, Some(b"0"), None)]);
    assert_eq!(
        requests(&mut rx[0])
            .iter()
            .map(|r| r.form_id)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    run(&mut state, vec![answer(1, 2, 1, Some(b"true"), None)]);
    assert_eq!(c.try_recv().ok(), Some(Response::Modal(true)));
}

// pmmp: Player::closeAllForms
#[test]
fn closing_forms_sends_the_close_and_forgets_them() {
    let (mut state, mut rx) = server();
    let mut c = send(&mut state, 1, menu());
    assert!(Context { state: &mut state }.close_forms(id(1)));
    assert!(!Context { state: &mut state }.close_forms(id(9)));
    run(&mut state, vec![answer(1, 1, 0, None, Some(0))]);
    let sent = packets(&mut rx[0]);
    assert!(matches!(sent.last(), Some(Packet::ClientBoundCloseForm(_))));
    assert!(calls(&mut c).is_empty());
    assert!(pending(&state, 1).is_empty());
}

#[test]
fn a_form_is_open_from_sending_until_it_is_answered_closed_or_closed_by_the_server() {
    let (mut state, _rx) = server();
    let is_open = |state: &mut State, session| Context { state }.has_form_open(id(session));
    assert!(!is_open(&mut state, 1), "nothing sent yet");
    assert!(!is_open(&mut state, 9), "not in the game");

    let _a = send(&mut state, 1, menu());
    let _b = send(&mut state, 1, menu());
    assert!(is_open(&mut state, 1));
    assert!(!is_open(&mut state, 2), "another player's form");
    run(&mut state, vec![answer(1, 1, 0, Some(b"0"), None)]);
    assert!(is_open(&mut state, 1), "one of two answered");
    run(&mut state, vec![answer(1, 2, 1, None, Some(0))]);
    assert!(!is_open(&mut state, 1), "the last closed by the player");

    let _c = send(&mut state, 1, menu());
    assert!(Context { state: &mut state }.close_forms(id(1)));
    assert!(!is_open(&mut state, 1), "closed by the server");

    let _d = send(&mut state, 1, menu());
    run(&mut state, vec![leave(1, 3)]);
    assert!(!is_open(&mut state, 1), "left");
}

#[test]
fn forms_sent_and_answered_repeatedly_leave_the_tables_as_they_started() {
    let (mut state, _rx) = server();
    assert_eq!(state.forms.counts(), (0, 0));
    for round in 0..100_u32 {
        let _c: Vec<Calls> = (0..16).map(|_| send(&mut state, 1, menu())).collect();
        let _d = send(&mut state, 2, menu());
        let seq = u64::from(round) * 20;
        let mut inputs: Vec<Input> = (0..16)
            .map(|i| answer(1, seq + u64::from(i), round * 16 + i, Some(b"0"), None))
            .collect();
        inputs.push(answer(2, seq, round, None, Some(0)));
        run(&mut state, inputs);
        assert_eq!(state.forms.counts(), (2, 0));
    }
    let mut cx = Context { state: &mut state };
    cx.set_scoreboard(id(1), Scoreboard::new(Slot::Sidebar, "t").line("a", 0))
        .unwrap();
    cx.set_boss_bar(id(1), BossBar::new("b")).unwrap();
    run(&mut state, vec![leave(1, 10_000), leave(2, 10_000)]);
    assert_eq!(state.forms.counts(), (0, 0));
}

fn scores(p: &Packet) -> Vec<(i64, u8, i32, String, i64)> {
    match p {
        Packet::SetScore(s) => s
            .entries
            .iter()
            .map(|e| {
                (
                    e.entry_id,
                    e.identity_type,
                    e.score,
                    e.display_name.clone(),
                    e.entity_unique_id,
                )
            })
            .collect(),
        p => panic!("not a SetScore: {p:?}"),
    }
}

#[test]
fn a_scoreboard_is_sent_whole_then_by_changed_lines() {
    let (mut state, mut rx) = server();
    let fake = u8::from(ScoreboardIdentity::FakePlayer);
    let gone = u8::from(ScoreboardIdentity::Remove);
    let player = u8::from(ScoreboardIdentity::Player);
    let rid2 = i64::try_from(state.players[&SessionId::new(2)].runtime_id).unwrap();
    let board = Scoreboard::new(Slot::Sidebar, "Lobby")
        .line("a", 1)
        .line("", 2)
        .player(id(2), 3);
    Context { state: &mut state }
        .set_scoreboard(id(1), board.clone())
        .unwrap();
    run(&mut state, vec![]);
    let sent = packets(&mut rx[0]);
    assert_eq!(sent.len(), 2);
    let Packet::SetDisplayObjective(d) = &sent[0] else {
        panic!("{:?}", sent[0]);
    };
    assert_eq!(
        (
            d.display_slot.as_str(),
            d.objective_name.as_str(),
            d.display_name.as_str()
        ),
        ("sidebar", "anchor:sidebar", "Lobby")
    );
    assert_eq!((d.criteria_name.as_str(), d.sort_order), ("dummy", 0));
    assert_eq!(
        scores(&sent[1]),
        [
            (-1025, fake, 1, "a".into(), 0),
            (-1026, fake, 2, "§2".into(), 0),
            (rid2, player, 3, String::new(), rid2),
        ]
    );
    for p in &sent {
        let mut bytes = Vec::new();
        p.encode(&mut bytes, 0).unwrap();
    }

    // The same scoreboard again sends nothing; a changed line is removed and added again.
    let mut cx = Context { state: &mut state };
    cx.set_scoreboard(id(1), board).unwrap();
    let changed = Scoreboard::new(Slot::Sidebar, "Lobby")
        .line("a", 1)
        .line("b", 2);
    cx.set_scoreboard(id(1), changed).unwrap();
    run(&mut state, vec![]);
    let sent = packets(&mut rx[0]);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        scores(&sent[0]),
        [
            (-1026, gone, 2, String::new(), 0),
            (rid2, gone, 3, String::new(), 0)
        ]
    );
    assert_eq!(scores(&sent[1]), [(-1026, fake, 2, "b".into(), 0)]);

    // A new title or sort order sends the whole scoreboard again.
    let mut cx = Context { state: &mut state };
    let sorted = Scoreboard::new(Slot::Sidebar, "Lobby")
        .sort(Sort::Descending)
        .line("a", 1);
    cx.set_scoreboard(id(1), sorted).unwrap();
    assert!(cx.remove_scoreboard(id(1), Slot::Sidebar));
    assert!(cx.remove_scoreboard(id(1), Slot::Sidebar));
    assert!(!cx.remove_scoreboard(id(9), Slot::Sidebar));
    run(&mut state, vec![]);
    let sent = packets(&mut rx[0]);
    let kinds: Vec<u32> = sent.iter().map(Packet::id).collect();
    assert_eq!(
        sent.iter()
            .map(|p| match p {
                Packet::RemoveObjective(r) => format!("remove {}", r.objective_name),
                Packet::SetDisplayObjective(d) => format!("display {}", d.sort_order),
                Packet::SetScore(s) => format!("score {}", s.entries.len()),
                p => format!("{p:?}"),
            })
            .collect::<Vec<_>>(),
        [
            "remove anchor:sidebar",
            "display 1",
            "score 1",
            "remove anchor:sidebar"
        ],
        "{kinds:?}"
    );
}

#[test]
fn a_scoreboard_line_cannot_name_a_player_still_loading() {
    let (mut state, _rx) = server();
    let board = || Scoreboard::new(Slot::List, "t").player(id(2), 0);
    let second = SessionId::new(2);
    let phase = state.players[&second].phase;
    state.players.get_mut(&second).expect("joined").phase = Phase::Loading;
    let mut cx = Context { state: &mut state };
    assert_eq!(
        cx.set_scoreboard(id(1), board()),
        Err(DisplayError::UnknownPlayer)
    );
    cx.state.players.get_mut(&second).expect("joined").phase = phase;
    assert!(cx.set_scoreboard(id(1), board()).is_ok());
}

#[test]
fn scoreboards_that_break_a_bound_are_refused() {
    let (mut state, _rx) = server();
    let mut cx = Context { state: &mut state };
    let lines =
        |n: usize, slot| (0..n).fold(Scoreboard::new(slot, "t"), |b, i| b.line(i.to_string(), 0));
    assert_eq!(
        cx.set_scoreboard(id(1), lines(16, Slot::Sidebar)),
        Err(DisplayError::TooManyLines)
    );
    assert!(cx.set_scoreboard(id(1), lines(15, Slot::Sidebar)).is_ok());
    assert!(cx.set_scoreboard(id(1), lines(1024, Slot::List)).is_ok());
    assert_eq!(
        cx.set_scoreboard(id(1), lines(1025, Slot::List)),
        Err(DisplayError::TooManyLines)
    );
    let long = "x".repeat(1025);
    assert_eq!(
        cx.set_scoreboard(id(1), Scoreboard::new(Slot::List, long.clone())),
        Err(DisplayError::TooLong)
    );
    assert_eq!(
        cx.set_scoreboard(
            id(1),
            Scoreboard::new(Slot::List, "t").line(long.clone(), 0)
        ),
        Err(DisplayError::TooLong)
    );
    assert_eq!(
        cx.set_scoreboard(id(1), Scoreboard::new(Slot::List, "t").player(id(9), 0)),
        Err(DisplayError::UnknownPlayer)
    );
    let twice = Scoreboard::new(Slot::List, "t")
        .player(id(2), 0)
        .player(id(2), 1);
    assert_eq!(
        cx.set_scoreboard(id(1), twice),
        Err(DisplayError::DuplicatePlayer)
    );
    assert_eq!(
        cx.set_scoreboard(id(9), Scoreboard::new(Slot::List, "t")),
        Err(DisplayError::Offline)
    );
    assert_eq!(
        cx.set_boss_bar(id(1), BossBar::new(long)),
        Err(DisplayError::TooLong)
    );
    assert_eq!(
        cx.set_boss_bar(id(9), BossBar::new("")),
        Err(DisplayError::Offline)
    );
}

#[test]
fn a_boss_bar_title_longer_than_a_client_decodes_is_refused() {
    let (mut state, _rx) = server();
    let mut cx = Context { state: &mut state };
    let max = crate::protocol::BOSS_EVENT_BOSS_BAR_TITLE_MAX_LEN;
    assert!(
        cx.set_boss_bar(id(1), BossBar::new("x".repeat(max)))
            .is_ok()
    );
    assert_eq!(
        cx.set_boss_bar(id(1), BossBar::new("x".repeat(max + 1))),
        Err(DisplayError::TooLong)
    );
}

#[test]
fn a_boss_bar_is_shown_on_the_players_own_entity_and_updated_by_what_changed() {
    let (mut state, mut rx) = server();
    let me = i64::try_from(state.players[&SessionId::new(1)].runtime_id).unwrap();
    let events = |rx: &mut mpsc::Receiver<Outbound>| -> Vec<(u8, String, f32, u8)> {
        packets(rx)
            .into_iter()
            .map(|p| match p {
                Packet::BossEvent(e) => {
                    assert_eq!(e.boss_entity_unique_id, me);
                    (
                        e.event_type,
                        e.boss_bar_title,
                        e.health_percentage,
                        e.colour,
                    )
                }
                p => panic!("{p:?}"),
            })
            .collect()
    };
    let (show, hide) = (u8::from(BossEventType::Show), u8::from(BossEventType::Hide));
    let title = u8::from(BossEventType::Title);
    let health = u8::from(BossEventType::HealthPercentage);
    let (purple, red) = (u8::from(Colour::Purple), u8::from(Colour::Red));
    let mut cx = Context { state: &mut state };
    cx.set_boss_bar(id(1), BossBar::new("a").progress(0.5))
        .unwrap();
    cx.set_boss_bar(id(1), BossBar::new("a").progress(0.5))
        .unwrap();
    cx.set_boss_bar(id(1), BossBar::new("b").progress(0.5))
        .unwrap();
    cx.set_boss_bar(id(1), BossBar::new("b").progress(2.0))
        .unwrap();
    cx.set_boss_bar(id(1), BossBar::new("b").colour(Colour::Red))
        .unwrap();
    assert!(cx.remove_boss_bar(id(1)));
    assert!(cx.remove_boss_bar(id(1)));
    assert!(!cx.remove_boss_bar(id(9)));
    run(&mut state, vec![]);
    assert_eq!(
        events(&mut rx[0]),
        [
            (show, "a".into(), 0.5, purple),
            (title, "b".into(), 0.5, purple),
            (health, "b".into(), 1.0, purple),
            (hide, "b".into(), 1.0, purple),
            (show, "b".into(), 1.0, red),
            (hide, "b".into(), 1.0, red),
        ]
    );
}
