//! The determinism run with listeners: the base script with listeners and monitors on every event,
//! busy listeners that cancel at a fixed pattern, change fields, call the direct API, defer and
//! perform actions, scheduled tasks and blocking jobs; its clients also chat, run commands and
//! answer forms. One and many pool threads must give the same ticks, and the input log (job results
//! included) must replay them. A run with idle listeners and monitors on every event must give the
//! ticks of the run without listeners, with the base script and with the script of API inputs.

use std::num::NonZeroU32;

use super::*;
use crate::action::{self, Chat, Damage, Kick};
use crate::event::{
    Actor, BlockBreak, BlockPlace, BlockUpdate, Command, CraftItem, DamageCause, EntityDamage,
    EntityItemPickup, InventoryTransaction, ItemSpawn, PlayerChat, PlayerDropItem, PlayerExhaust,
    PlayerInteract, PlayerItemHeld, PlayerItemUse, PlayerJoin, PlayerMove, PlayerQuit,
    PlayerToggleSneak, ProjectileLaunch, Trade, TradeOpen,
};
use crate::form::{Modal, Response};
use crate::protocol::consts::TextType;
use crate::protocol::packet::{CommandRequest, ModalFormResponse, Text};
use crate::protocol::{CommandOrigin, Rng};

/// Form answers: the first and second button, closed, malformed and not JSON.
const ANSWERS: [Option<&[u8]>; 5] = [
    Some(b"true"),
    Some(b"false"),
    None,
    Some(b"[1]"),
    Some(b"{oops"),
];

/// What the API inputs must have reached over all seeds, besides [`REACHED`].
const BUSY_REACHED: [&str; 1] = ["form"];

/// Seeds of the run with listeners: two keep it near the base run's time, with more work per tick.
const BUSY_SEEDS: [u64; 2] = [SEEDS[0], SEEDS[2]];

/// Chat lines the clients send: plain, a command typed in chat, one a listener kicks for.
const CHAT: [&str; 5] = ["hello", "gg", "/me dances", "/list", "bye"];

/// Command lines the clients send: allowed to everyone, operator only (refused: an operator
/// change asks for a save, and these runs answer only their first), malformed and unknown.
const COMMANDS: [&str; 10] = [
    "/list",
    "/me waves",
    "/tell p3 hello",
    "/help",
    "/say hi",
    "/give @s apple 2",
    "/xp 5",
    "/tp 1 -60 1",
    "/gamemode banana",
    "/nothing",
];

/// The most unanswered forms the script remembers per session (the server keeps 16).
pub(super) const MAX_REMEMBERED_FORMS: usize = 32;

/// A script made from a seed and a number of sessions.
type MakeScript = fn(u64, usize) -> Script;

/// One API input of a client: a chat line, a command, or an answer to one of `forms` (sometimes to
/// a form already answered or never sent).
pub(super) fn client_input(rng: &mut Rng, forms: &mut Vec<u32>) -> Packet {
    match rng.below(4) {
        0 => Packet::Text(Box::new(Text {
            text_type: TextType::Chat.into(),
            source_name: "someone".to_owned(),
            message: CHAT[rng.below(CHAT.len())].to_owned(),
            xuid: "123".to_owned(),
            ..Text::default()
        })),
        1 => Packet::CommandRequest(CommandRequest {
            command_line: COMMANDS[rng.below(COMMANDS.len())].to_owned(),
            command_origin: CommandOrigin {
                origin: 0,
                uuid: crate::protocol::Uuid::from_bytes([3; 16]),
                request_id: "req".to_owned(),
                player_unique_id: 0,
            },
            internal: false,
            version: String::new(),
        }),
        _ => {
            let form_id = match (forms.is_empty(), rng.below(8)) {
                (true, _) | (false, 0) => u32::try_from(rng.below(40)).unwrap(),
                (false, 1) => forms[0],
                (false, _) => forms.remove(rng.below(forms.len())),
            };
            let answer = ANSWERS[rng.below(ANSWERS.len())];
            Packet::ModalFormResponse(ModalFormResponse {
                form_id,
                response_data: answer.map(<[u8]>::to_vec),
                cancel_reason: answer.is_none().then_some(0),
            })
        }
    }
}

/// The results of some of the blocking jobs that wait, as the blocking pool would deliver them:
/// the JSON of a number, or a panic.
pub(super) fn job_results(rng: &mut Rng, state: &State) -> Vec<Input> {
    let mut out = Vec::new();
    for id in state.tasks.waiting_jobs() {
        if rng.below(3) != 0 {
            continue;
        }
        let outcome = if rng.below(8) == 0 {
            Err(crate::task::Error::Panicked)
        } else {
            Ok(serde_json::to_vec(&id.wrapping_mul(3)).unwrap())
        };
        out.push(Input {
            session: SessionId::JOBS,
            seq: id,
            kind: InputKind::Job(outcome),
        });
    }
    out
}

/// Whether the `n`th call of a listener is one of every `k`th.
fn every(n: u32, k: u32) -> bool {
    n.is_multiple_of(k)
}

/// The run's listeners: idle listeners and monitors on every event, then busy ones (sorted).
#[expect(
    clippy::too_many_lines,
    reason = "consistency: the run's listeners in one place"
)]
fn busy() -> Listeners {
    let mut l = Listeners::default();
    l.listen_to_every_event(|_| {});
    l.monitor_every_event(|_| {});
    let dirt = || block_item(crate::block::Kind::Dirt, 1);
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut BlockBreak, cx| {
        n += 1;
        if every(n, 5) {
            e.cancel();
        } else if every(n, 3) {
            e.set_drops(vec![dirt()]);
        }
        if every(n, 7) {
            // A changed block fails the second check: the break is cancelled.
            let stone = crate::block::State::find("minecraft:stone", &[]).unwrap();
            assert!(cx.default_world_mut().set_block(e.pos(), stone).is_ok());
        }
        if every(n, 11)
            && let Some(pos) = e.pos().step(crate::block::Face::Down)
        {
            let player = e.player();
            cx.perform(action::BreakBlock::new(player, pos)).unwrap();
        }
    });
    let mut n = 0;
    l.push(Priority::High, move |e: &mut BlockPlace, cx| {
        n += 1;
        if every(n, 4) {
            e.cancel();
        }
        if every(n, 6) {
            let player = e.player();
            cx.defer(move |cx| {
                if let Some(p) = cx.player_mut(player) {
                    p.inventory_mut(crate::inventory::Kind::Main).add(&dirt());
                }
            })
            .unwrap();
        }
    });
    let mut n = 0;
    l.push(Priority::Low, move |e: &mut PlayerInteract, _| {
        n += 1;
        if every(n, 9) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut InventoryTransaction, _| {
        n += 1;
        if every(n, 11) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerDropItem, _| {
        n += 1;
        if every(n, 3) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut EntityDamage, cx| {
        n += 1;
        if every(n, 4) {
            e.cancel();
            return;
        }
        if every(n, 3) {
            e.set_base_damage(e.base_damage() / 2.0);
        }
        if every(n, 5) {
            e.set_knockback(0.0);
        }
        if every(n, 8)
            && e.cause() != DamageCause::Custom
            && let Actor::Player(player) = e.entity()
        {
            cx.perform(Damage::new(player, 1.0, DamageCause::Custom))
                .unwrap();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerMove, _| {
        n += 1;
        if every(n, 13) {
            e.cancel();
        } else if every(n, 17) {
            e.set_target(e.origin());
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerChat, cx| {
        n += 1;
        if e.message().contains("bye") {
            cx.perform(Kick::new(e.player(), "bye")).unwrap();
        }
        if every(n, 3) {
            e.cancel();
        } else {
            e.set_message(format!("[{n}] {}", e.message()));
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut Command, _| {
        n += 1;
        if every(n, 5) {
            e.cancel();
        } else if every(n, 7) {
            e.set_command("list".to_owned());
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerJoin, cx| {
        n += 1;
        let player = e.player();
        e.set_join_message(format!("welcome {n}"));
        let modal = Modal::new("Kit", "Take a kit?", "Yes", "No");
        let sent = cx.send_form(player, modal, move |cx, player, response| {
            if response == Response::Modal(true)
                && let Some(p) = cx.player_mut(player)
            {
                let bread = crate::item::Stack::new(crate::item::Kind::Bread.default_type(), 2);
                p.inventory_mut(crate::inventory::Kind::Main).add(&bread);
            }
        });
        assert!(sent.is_ok());
        // Every 7 ticks a little exhaustion, six times or until the player leaves.
        let mut runs = 0;
        cx.schedule_repeating(NonZeroU32::new(7).unwrap(), move |cx| {
            runs += 1;
            let online = match cx.player_mut(player) {
                Some(p) => {
                    p.exhaust(0.5).unwrap();
                    true
                }
                None => false,
            };
            if !online || runs >= 6 {
                let me = cx.current_task().unwrap();
                assert!(cx.cancel_task(me));
            }
        })
        .unwrap();
        cx.schedule(20, move |cx| {
            if cx.player(player).is_some() {
                cx.perform(Damage::new(player, 2.0, DamageCause::Magic))
                    .unwrap();
            }
        })
        .unwrap();
        let cancelled = cx.schedule(5, |_| panic!("a cancelled task ran")).unwrap();
        assert!(cx.cancel_task(cancelled));
        // The work never runs in these runs: the script delivers its result, and the replay
        // takes it from the log.
        cx.spawn_blocking(
            || 0_u64,
            move |cx, result: crate::task::Result<u64>| {
                if let (Ok(v), Some(p)) = (result, cx.player_mut(player)) {
                    p.add_xp(u32::try_from(v % 7).unwrap());
                }
            },
        )
        .unwrap();
    });
    l.push_monitor(|_: &PlayerQuit, cx| {
        assert!(cx.player_count() <= 64);
    });
    l.push_monitor(|e: &ItemSpawn, cx| {
        assert!(cx.entity(e.entity()).is_some());
    });
    l.push_monitor(|e: &BlockUpdate, cx| {
        assert!(cx.default_world().block(e.pos()).is_some());
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut EntityItemPickup, _| {
        n += 1;
        if every(n, 5) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut CraftItem, _| {
        n += 1;
        if every(n, 4) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerItemHeld, _| {
        n += 1;
        if every(n, 6) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerItemUse, _| {
        n += 1;
        if every(n, 7) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut PlayerExhaust, _| {
        n += 1;
        if every(n, 2) {
            e.set_amount(e.amount() * 2.0);
        }
    });
    l.push(Priority::Normal, |e: &mut PlayerToggleSneak, cx| {
        if e.is_sneaking() {
            cx.perform(Chat::new(e.player(), "sneaky")).unwrap();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut ProjectileLaunch, _| {
        n += 1;
        if every(n, 3) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut TradeOpen, _| {
        n += 1;
        if every(n, 4) {
            e.cancel();
        }
    });
    let mut n = 0;
    l.push(Priority::Normal, move |e: &mut Trade, _| {
        n += 1;
        if every(n, 2) {
            e.cancel();
        }
    });
    l
}

/// Idle listeners and monitors on every event: every event is built and checked again.
pub(super) fn idle() -> Listeners {
    let mut l = Listeners::default();
    l.listen_to_every_event(|_| {});
    l.monitor_every_event(|_| {});
    l
}

#[test]
fn busy_listeners_tasks_and_jobs_give_the_same_ticks_on_one_and_many_threads_and_replay() {
    let many = crate::runtime::logical_cpus().max(4);
    let runs = side_by_side(&BUSY_SEEDS, |&seed| {
        let (one, log_one, seen) = live(1, Script::busy(seed, 64), busy());
        let (more, log_more, seen_more) = live(many, Script::busy(seed, 64), busy());
        assert_eq!(one, more, "seed {seed:#x}: 1 and {many} threads");
        assert_eq!(seen, seen_more, "seed {seed:#x}");
        let bytes = log_one.encode().unwrap();
        assert_eq!(bytes, log_more.encode().unwrap(), "seed {seed:#x}");
        let jobs = log_one
            .ticks()
            .iter()
            .flat_map(|t| &t.inputs)
            .filter(|e| matches!(e.kind, EntryKind::Job(_)))
            .count();
        let live_hashes: Vec<u64> = one.iter().map(|(h, _)| *h).collect();
        let read = InputLog::decode(&bytes).unwrap();
        assert!(!read.is_truncated());
        for threads in [1, many] {
            let pool = crate::runtime::cpu_pool(threads).unwrap();
            let (hashes, again) = replay(&read.ticks(), busy(), &pool);
            assert_eq!(hashes, live_hashes, "seed {seed:#x}: replay");
            assert_eq!(
                again.encode().unwrap(),
                bytes,
                "seed {seed:#x}: re-recorded"
            );
            assert!(pool.shutdown());
        }
        (seen, jobs)
    });
    let mut reached = client::Seen::new();
    let mut jobs = 0;
    for (seen, n) in runs {
        for (what, n) in seen {
            *reached.entry(what).or_default() += n;
        }
        jobs += n;
    }
    assert!(jobs > 0, "no job result was logged");
    // Without the vanilla block rules, only the base run's longer scripts make another entity.
    let wanted = REACHED.iter().filter(|what| **what != "entity");
    for what in wanted.chain(&BUSY_REACHED) {
        assert!(
            reached.get(what).is_some_and(|n| *n > 0),
            "the scripts never reached {what}: {reached:?}"
        );
    }
}

#[test]
fn idle_listeners_and_monitors_on_every_event_leave_the_ticks_unchanged() {
    let threads = crate::runtime::logical_cpus().max(4);
    // The base script, then the script with the API inputs (chat, commands, form answers, job
    // results), which pass the paths a listener changes.
    let scripts: [(u64, MakeScript); 3] = [
        (SEEDS[1], Script::new),
        (BUSY_SEEDS[0], Script::busy),
        (BUSY_SEEDS[1], Script::busy),
    ];
    side_by_side(&scripts, |&(seed, script)| {
        let (without, log_without, seen) = live(threads, script(seed, 64), Listeners::default());
        let (with, log_with, seen_with) = live(threads, script(seed, 64), idle());
        assert_eq!(with.len(), without.len(), "seed {seed:#x}");
        for (tick, (a, b)) in with.iter().zip(&without).enumerate() {
            assert_eq!(a, b, "seed {seed:#x}: tick {tick}");
        }
        assert_eq!(seen_with, seen, "seed {seed:#x}");
        assert_eq!(
            log_with.encode().unwrap(),
            log_without.encode().unwrap(),
            "seed {seed:#x}"
        );
    });
}
