//! Forced shutdown with item flows: a server keeps moving items between a player's inventory, a
//! chest and item entities on the ground, and is killed again and again, at moments counted in
//! ticks from the commits it reports (never in wall-clock time). Every other run saves every third
//! tick and is killed a random number of ticks after its first commit, during a later commit or
//! between two; the runs between commit once, at a phase of the item cycle taken by turns (items on
//! the ground, on the player's cursor, or in the slots and the chest
//! only), and are killed after that commit, so the commits caught hold items in every place however
//! slow the machine is. Every restart must find undamaged data files whose last commit is newer
//! than the last restart's and that hold every item exactly once (no duplicate, no loss) and hold
//! the player, the chest, the ground and the terrain of the main file's last save, also after a
//! kill between the world file's commit and the players file's or the main file's.
//!
//! The test runs its own test binary as the server process (`item_crash_child`) and kills only that
//! child, through its process handle. The child is the real server without the network (see
//! `task::crash::serve`): the server file, the save thread and the tick thread, and one player who
//! joins through the tick's input queue as a session would. The items move by a scheduled task through
//! the rule modules' own functions; a player client is not needed for what is tested here, which is
//! that one commit holds every part of the state.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::block::{Face, Kind as BlockKind};
use crate::event::{Listeners, Priority, WorldSave};
use crate::item::{Kind as ItemKind, Stack, Type};
use crate::math::Vec3;
use crate::nbt::{Compound, List, Value};
use crate::protocol::packet::{ItemStackRequest as RequestPacket, Packet, RequestChunkRadius};
use crate::protocol::{
    FullContainerName, ItemStackRequest, Rng, StackRequestAction, StackRequestSlotInfo,
    TakeStackRequestAction, TransferStackRequestAction,
};
use crate::storage::ChunkKind;
use crate::tick::queue::Input;
use crate::tick::task::crash::{
    CHILD_TICKS, Running, SESSION, fields, fresh, gold, path, remove, report, serve, spawn_child,
    stop_server,
};
use crate::tick::{
    InputLog, Output, Setup, State, Tasks, block_entity, boundary, deliver, entity, inventory,
    save, task,
};
use crate::world::{BlockPos, ChunkPos};

/// The child's environment variable naming the phase of the item cycle (the tick modulo
/// [`CYCLE_TICKS`], see [`step`]) its run commits once, after [`SETTLE`] ticks of moves; without
/// it the run commits every third tick.
const AIM_ENV: &str = "ANCHOR_ITEM_CRASH_AIM";
/// The phases the aimed runs commit, by turns: items on the ground, on the cursor, in the slots
/// and the chest only (see [`step`] for what each phase of the cycle does).
const AIMS: [u64; 5] = [2, 3, CURSOR_PHASE, 9, 0];
/// The chest, in a spawn chunk (always loaded).
const CHEST: BlockPos = BlockPos::new(4, -60, 4);
/// The phase of the item cycle at which sticks go onto the cursor.
const CURSOR_PHASE: u64 = 6;
/// Ticks of one cycle of the item moves.
const CYCLE_TICKS: u64 = 12;
/// The child's environment variable naming its data folder.
const DATA_ENV: &str = "ANCHOR_ITEM_CRASH_DATA";
/// Where items are dropped: in a spawn chunk, far from the player at the spawn, so it never
/// picks them up.
const DROP: Vec3 = Vec3::new(12.5, -59.5, 12.5);
/// The items and how many of each the setup gives the player.
const ITEMS: [(ItemKind, u64); 2] = [(ItemKind::Diamond, 100), (ItemKind::Stick, 200)];
/// Kills, each followed by a restart that checks the file.
const KILLS: u32 = 50;
/// The mode of a child that only reports what it started with.
const MODE_CHECK: &str = "check";
/// The child's environment variable naming its mode: [`MODE_RUN`] or [`MODE_CHECK`].
const MODE_ENV: &str = "ANCHOR_ITEM_CRASH_MODE";
/// The mode of a child that keeps moving items.
const MODE_RUN: &str = "run";
/// The player's name.
const NAME: &str = "crasher";
/// The prefix of the child's report lines.
const REPORT: &str = "ANCHOR-ITEMS";
/// How long the parent waits for the next report line of the child.
const REPORT_TIMEOUT: Duration = Duration::from_secs(120);
/// Ticks of moves before an aimed commit, so that items are in every place of the cycle.
const SETTLE: u64 = 24;

fn item(i: usize) -> Type {
    ITEMS[i].0.default_type()
}

/// Items of each kind of [`ITEMS`].
type Counts = [u64; 2];

fn add(counts: &mut Counts, stack: &Stack) {
    for (i, c) in counts.iter_mut().enumerate() {
        if stack.item_type() == Some(item(i)) {
            *c += u64::from(stack.count());
        }
    }
}

/// Every stack of a saved item list (other keys of an entry, such as `Slot`, are ignored).
fn stacks(list: &[Compound]) -> impl Iterator<Item = Stack> + '_ {
    list.iter().filter_map(Stack::from_nbt)
}

/// What the player holds, read from the data a save writes of it.
fn player_counts(state: &State) -> Option<Counts> {
    let player = state.players.get(&SESSION)?;
    let data = save::data_of(state, player);
    let mut counts = [0; 2];
    for key in [
        "Inventory",
        "Armor",
        "Offhand",
        "EnderChestInventory",
        "PendingItems",
    ] {
        if let Some(Value::List(List::Compound(list))) = data.extra.get(key) {
            for s in stacks(list) {
                add(&mut counts, &s);
            }
        }
    }
    Some(counts)
}

/// What the block entities of the loaded chunks hold.
fn chest_counts(state: &State) -> Counts {
    let mut counts = [0; 2];
    for s in state.rules.block_entities.all_items() {
        add(&mut counts, s);
    }
    counts
}

/// The item entities of the loaded chunks as a save writes them: the rules' changed lists go to
/// the chunk extras first, as every snapshot does.
pub(super) fn ground(state: &mut State) -> Vec<Stack> {
    state.rules.flush(&mut state.chunks.extras);
    let mut out = Vec::new();
    for pos in state.worlds[0].loaded_chunks() {
        let Some(extras) = state.chunks.extras.get((0, pos)) else {
            continue;
        };
        for c in extras.get(ChunkKind::Entities) {
            if c.get("identifier") == Some(&Value::String("minecraft:item".to_owned()))
                && let Some(Value::Compound(stack)) = c.get("Item")
                && let Some(stack) = Stack::from_nbt(stack)
            {
                out.push(stack);
            }
        }
    }
    out
}

fn ground_counts(state: &mut State) -> Counts {
    let mut counts = [0; 2];
    for s in ground(state) {
        add(&mut counts, &s);
    }
    counts
}

fn text(c: Counts) -> String {
    format!("{},{}", c[0], c[1])
}

/// Takes up to `n` items of kind `i` out of the player's inventory.
fn take_player(state: &mut State, i: usize, n: u8) -> u8 {
    let Some(player) = state.players.get_mut(&SESSION) else {
        return 0;
    };
    let mut taken = 0;
    let mut rest = Vec::new();
    for s in inventory::take_all(player) {
        if s.item_type() == Some(item(i)) && taken < n {
            let k = (n - taken).min(s.count());
            taken += k;
            rest.push(s.with_count(s.count() - k));
        } else {
            rest.push(s);
        }
    }
    for s in rest.into_iter().filter(|s| !s.is_empty()) {
        assert!(
            inventory::give(player, s).is_empty(),
            "the inventory has room"
        );
    }
    taken
}

fn give_player(state: &mut State, stack: Stack) {
    let player = state.players.get_mut(&SESSION).expect("the player is in");
    assert!(
        inventory::give(player, stack).is_empty(),
        "the inventory has room"
    );
}

/// Puts `stack` into the chest: onto stacks of its item, then into empty slots.
fn put_chest(state: &mut State, mut stack: Stack) {
    let chest = state
        .rules
        .block_entities
        .get_mut(0, CHEST)
        .expect("the chest is there");
    for pass in [false, true] {
        for slot in &mut chest.items {
            if stack.is_empty() {
                return;
            }
            let fits = if pass {
                slot.is_empty()
            } else {
                slot.can_stack_with(&stack)
            };
            if !fits {
                continue;
            }
            let max = stack
                .max_stack_size()
                .expect("a stack that is not empty has a size");
            let room = max - slot.count().min(max);
            let k = room.min(stack.count());
            *slot = stack.with_count(slot.count() + k);
            stack = stack.with_count(stack.count() - k);
        }
    }
    assert!(stack.is_empty(), "the chest has room");
}

/// Takes up to `n` items of kind `i` out of the chest.
fn take_chest(state: &mut State, i: usize, n: u8) -> u8 {
    let chest = state
        .rules
        .block_entities
        .get_mut(0, CHEST)
        .expect("the chest is there");
    let mut taken = 0;
    for slot in &mut chest.items {
        if slot.item_type() == Some(item(i)) && taken < n {
            let k = (n - taken).min(slot.count());
            taken += k;
            *slot = slot.with_count(slot.count() - k);
        }
    }
    taken
}

/// Drops `stack` at [`DROP`]; false (nothing dropped) at the entity limit.
fn drop_stack(state: &mut State, stack: Stack) -> bool {
    stack.is_empty() || entity::drop_item(state, 0, DROP, stack)
}

/// Takes every item entity off the ground, if each has its runtime ID yet (loaded ones get it at
/// their first entity tick); `None` otherwise.
fn take_ground(state: &mut State) -> Option<Vec<Stack>> {
    let stacks = ground(state);
    let ids: Vec<_> = (1..state.next_runtime_id)
        .filter_map(|r| entity::find(state, r))
        .collect();
    if ids.len() != stacks.len() {
        return None;
    }
    for id in ids {
        assert!(entity::remove(state, id));
    }
    Some(stacks)
}

/// Makes the moves between the player's slots, the chest and the ground (step `k`, 0 to 5).
fn moves(state: &mut State, k: u64, tick: u64) {
    match k {
        0 => {
            let n = take_player(state, 1, 9);
            if n > 0 {
                put_chest(state, Stack::new(item(1), n));
            }
        }
        1 => {
            let n = take_player(state, 0, 2);
            if n > 0 {
                put_chest(state, Stack::new(item(0), n));
            }
        }
        2 => {
            let n = take_chest(state, 1, 5);
            let stack = Stack::new(item(1), n);
            if n > 0 && !drop_stack(state, stack.clone()) {
                put_chest(state, stack);
            }
        }
        3 => {
            for (from_player, i) in [(true, 0), (false, 0), (true, 1)] {
                let n = if from_player {
                    take_player(state, i, 1)
                } else {
                    take_chest(state, i, 1)
                };
                let stack = Stack::new(item(i), n);
                if n > 0 && !drop_stack(state, stack.clone()) {
                    if from_player {
                        give_player(state, stack);
                    } else {
                        put_chest(state, stack);
                    }
                }
            }
        }
        4 => {
            if let Some(stacks) = take_ground(state) {
                let to_chest = (tick / CYCLE_TICKS).is_multiple_of(2);
                for s in stacks {
                    if to_chest {
                        put_chest(state, s);
                    } else {
                        give_player(state, s);
                    }
                }
            }
        }
        _ => {
            for i in 0..2 {
                let n = take_chest(state, i, 4);
                if n > 0 {
                    give_player(state, Stack::new(item(i), n));
                }
            }
        }
    }
}

/// Makes the item moves of one tick, by the tick: player → chest, chest → ground, player →
/// ground, ground → chest or player, chest → player, then main slots → the cursor, held there for
/// five ticks (several commits), and back.
fn step(state: &mut State, tick: u64) {
    match tick % CYCLE_TICKS {
        CURSOR_PHASE => {
            let sticks = take_player(state, 1, 3);
            let player = state.players.get_mut(&SESSION).expect("the player is in");
            inventory::set_cursor(player, Stack::new(item(1), sticks));
        }
        7..=10 => {}
        // Closing (no window is open) gives the cursor back.
        11 => inventory::close_window(state, SESSION),
        k => moves(state, k, tick),
    }
}

/// The child's task state.
struct Mover {
    moves: bool,
    /// The phase of the run's only commit ([`AIM_ENV`]).
    aim: Option<u64>,
    /// The tick of that commit, chosen at the first move.
    target: Option<u64>,
    stop: mpsc::Sender<()>,
    /// The tick the server started at: its last commit's.
    start: Option<u64>,
    /// Whether the state was reported and the moves may start.
    ready: bool,
    /// Where the marker is.
    marker: Option<BlockPos>,
}

impl Mover {
    fn tick(&mut self, cx: &mut task::Context<'_>) {
        let tick = cx.tick();
        let start = *self.start.get_or_insert(tick);
        if tick > start + CHILD_TICKS {
            stop_server(&self.stop);
            return;
        }
        let state = &mut *cx.state;
        if !self.ready {
            self.ready = self.check(state, start);
            if self.ready && !self.moves {
                stop_server(&self.stop);
            }
            return;
        }
        if !self.moves {
            return;
        }
        if state.rules.block_entities.get(0, CHEST).is_none() {
            // Nothing was committed yet: the chest and the player's items, once.
            state.worlds[0]
                .set_block(CHEST, BlockKind::Chest.default_state())
                .unwrap();
            block_entity::placed(state, 0, CHEST, None, &Stack::EMPTY, Face::Up);
            for (i, (_, n)) in ITEMS.into_iter().enumerate() {
                let mut left = n;
                while left > 0 {
                    let k = u8::try_from(left.min(64)).unwrap();
                    give_player(state, Stack::new(item(i), k));
                    left -= u64::from(k);
                }
            }
        }
        step(state, tick);
        let world = &mut state.worlds[0];
        if let Some(old) = self.marker {
            world
                .set_block(old, BlockKind::Dirt.default_state())
                .unwrap();
        }
        world.set_block(path(tick), gold()).unwrap();
        self.marker = Some(path(tick));
        state
            .rules
            .block_entities
            .get_mut(0, CHEST)
            .expect("the chest is there")
            .name = Some(format!("t{tick}"));
        let player = player_counts(state).expect("the player is in");
        let chest = chest_counts(state);
        let on_ground = ground_counts(state);
        report(
            REPORT,
            &format!(
                "tick={tick} player={} chest={} ground={}",
                text(player),
                text(chest),
                text(on_ground)
            ),
        );
        let save = match self.aim {
            Some(phase) => {
                let target = *self.target.get_or_insert_with(|| {
                    let settled = tick + SETTLE;
                    settled + (phase + CYCLE_TICKS - settled % CYCLE_TICKS) % CYCLE_TICKS
                });
                tick == target
            }
            // Every third tick, and when items go onto the cursor.
            None => tick.is_multiple_of(3) || tick % CYCLE_TICKS == CURSOR_PHASE,
        };
        if save {
            cx.save();
        }
    }

    /// Reports what the server started with once the spawn chunks and the player's data are in
    /// and every item entity has its runtime ID; false while waiting.
    fn check(&mut self, state: &mut State, start: u64) -> bool {
        let world = &state.worlds[0];
        let loaded = (-1..=1).all(|x| (-1..=1).all(|z| world.is_chunk_loaded(ChunkPos::new(x, z))));
        let joined = state
            .players
            .get(&SESSION)
            .is_some_and(|p| p.phase != crate::tick::Phase::Loading);
        if !loaded || !joined {
            return false;
        }
        let entities = (1..state.next_runtime_id)
            .filter(|&r| entity::find(state, r).is_some())
            .count();
        if entities != ground(state).len() {
            return false;
        }
        let world = &state.worlds[0];
        let mut golds = Vec::new();
        for x in -16..32 {
            for z in -16..32 {
                let at = BlockPos::new(x, -62, z);
                if world.block(at) == Some(gold()) {
                    golds.push(format!("{x},{z}"));
                }
            }
        }
        self.marker = golds.first().and_then(|g| {
            let (x, z) = g.split_once(',')?;
            Some(BlockPos::new(x.parse().ok()?, -62, z.parse().ok()?))
        });
        let chest = state
            .rules
            .block_entities
            .get(0, CHEST)
            .map_or("-".to_owned(), |c| c.name.clone().unwrap_or_default());
        let quarantined = world.quarantined().len();
        // What the commit held outside the player's slots (back in them since the load).
        let mut carried = [0; 2];
        if let Some(Value::List(List::Compound(list))) = state
            .players
            .get(&SESSION)
            .and_then(|p| p.extra.raw().get("PendingItems"))
        {
            for s in stacks(list) {
                add(&mut carried, &s);
            }
        }
        let player = player_counts(state).expect("the player is in");
        let in_chest = chest_counts(state);
        let on_ground = ground_counts(state);
        report(
            REPORT,
            &format!(
                "start={start} golds={} name={chest} quarantined={quarantined} carried={} \
                 player={} chest={} ground={}",
                golds.join(";"),
                text(carried),
                text(player),
                text(in_chest),
                text(on_ground)
            ),
        );
        true
    }
}

/// The server process the parent test starts and kills. Without the parent's environment it
/// does nothing.
#[test]
#[ignore = "the child process of forced_kills_never_duplicate_lose_or_split_items"]
fn item_crash_child() {
    let (Ok(data), Ok(mode)) = (std::env::var(DATA_ENV), std::env::var(MODE_ENV)) else {
        return;
    };
    let (stop_tx, stop) = mpsc::channel(1);
    let mut mover = Mover {
        moves: mode == MODE_RUN,
        aim: std::env::var(AIM_ENV).ok().map(|a| a.parse().unwrap()),
        target: None,
        stop: stop_tx,
        start: None,
        ready: false,
        marker: None,
    };
    let mut tasks = Tasks::default();
    tasks.push(NonZeroU32::MIN, move |cx| mover.tick(cx));
    // The parent times its kill from the commits.
    let mut listeners = Listeners::default();
    listeners.push(
        Priority::Normal,
        |_: &mut WorldSave, _: &mut task::Context<'_>| report(REPORT, "committed"),
    );
    let setup = Setup {
        listeners,
        tasks,
        ..Setup::default()
    };
    // The player joins and asks for its chunks, as a session would; it never spawns, so it
    // stands still at the spawn and never picks anything up.
    let radius = packet(Packet::RequestChunkRadius(RequestChunkRadius {
        chunk_radius: 2,
        max_chunk_radius: 0,
    }));
    serve(Path::new(&data), NAME, 3, setup, stop, vec![radius]);
}

fn start(data: &Path, mode: &str, aim: Option<u64>) -> Running {
    let aim = aim.map(|phase| phase.to_string());
    let mut envs = vec![(DATA_ENV, data.as_os_str()), (MODE_ENV, OsStr::new(mode))];
    if let Some(aim) = &aim {
        envs.push((AIM_ENV, OsStr::new(aim)));
    }
    spawn_child("tick::tests::crash::item_crash_child", REPORT, &envs)
}

fn counts(text: &str) -> Counts {
    let (a, b) = text.split_once(',').unwrap();
    [a.parse().unwrap(), b.parse().unwrap()]
}

/// Where the items were at each tick a run reported: player, chest, ground.
type Places = [Counts; 3];

fn places(f: &BTreeMap<String, String>) -> Places {
    [
        counts(&f["player"]),
        counts(&f["chest"]),
        counts(&f["ground"]),
    ]
}

/// Records the tick lines of a run: a later run's line for a tick replaces an earlier one's,
/// since a restart goes on from the commit, not from where the killed run was.
fn record(line: &str, ticks: &mut BTreeMap<u64, Places>) {
    let f = fields(line);
    if let Some(t) = f.get("tick") {
        ticks.insert(t.parse().unwrap(), places(&f));
    }
}

/// The child's first report: what the server started with.
fn first_report(server: &mut Running, kill: u32) -> String {
    server
        .lines
        .recv_timeout(REPORT_TIMEOUT)
        .unwrap_or_else(|_| {
            let status = server.child.try_wait().unwrap();
            panic!("kill {kill}: no report (exit {status:?}): a damaged file is refused at start")
        })
}

/// Kills the child `after` ticks after its first commit (an aimed run's only one), counted by
/// its reports however slow the machine is, recording the tick lines read on the way.
fn kill_after_commit(
    server: &mut Running,
    kill: u32,
    after: u64,
    ticks: &mut BTreeMap<u64, Places>,
) {
    let mut since_commit = None;
    while since_commit.is_none_or(|n| n < after) {
        let line = server
            .lines
            .recv_timeout(REPORT_TIMEOUT)
            .unwrap_or_else(|_| panic!("kill {kill}: the child stopped reporting"));
        record(&line, ticks);
        if line == "committed" {
            since_commit.get_or_insert(0);
        } else if let Some(n) = &mut since_commit
            && line.starts_with("tick=")
        {
            *n += 1;
        }
    }
    server.child.kill().unwrap();
}

/// The data folder of the chain whose first kill is `first`.
fn data_dir(first: u32) -> PathBuf {
    std::env::temp_dir().join(format!("anchor-item-crash-{}-{first}", std::process::id()))
}

/// What the restarts of a chain found.
#[derive(Default)]
struct Found {
    /// Restarts that found items in the player, the chest and on the ground.
    held: [u32; 3],
    /// Restarts that found items on the cursor.
    carried: u32,
    /// Restarts that found a newer commit than the restart before.
    advanced: u32,
}

/// Kills `first` to `end` (not included), each `afters[kill]` ticks after a commit, on a world
/// file of their own, then a last restart that only checks.
fn chain(first: u32, end: u32, afters: &[u64]) -> Found {
    let data = data_dir(first);
    fresh(&data);
    let mut ticks: BTreeMap<u64, Places> = BTreeMap::new();
    let mut committed = false;
    let mut found = Found::default();
    let mut last_start = 0;
    // The phase the last run committed, if it was aimed.
    let mut aimed = None;
    for kill in first..=end {
        let mode = if kill == end { MODE_CHECK } else { MODE_RUN };
        let aim = (mode == MODE_RUN && kill.is_multiple_of(2))
            .then(|| AIMS[usize::try_from(kill / 2).unwrap() % AIMS.len()]);
        let mut server = start(&data, mode, aim);
        let line = first_report(&mut server, kill);
        let f = fields(&line);
        let start: u64 = f["start"].parse().unwrap();
        assert_eq!(
            f["quarantined"], "0",
            "kill {kill}: a damaged chunk (start {start})"
        );
        let in_places = places(&f);
        let total: Counts = std::array::from_fn(|i| in_places.iter().map(|p| p[i]).sum());
        // A kill between the files of one save is rolled back by the start: every file holds the
        // main file's last save, exactly.
        if f["name"] == "-" {
            // Nothing was committed: no chest, no items anywhere, no marker.
            assert!(
                !committed,
                "kill {kill}: the commit was lost (start {start})"
            );
            assert_eq!(total, [0, 0], "kill {kill}: items without their chest");
            assert_eq!(f["golds"], "", "kill {kill}: a marker without the chest");
        } else {
            committed = true;
            for (h, place) in found.held.iter_mut().zip(&in_places) {
                if place.iter().any(|n| *n > 0) {
                    *h += 1;
                }
            }
            if counts(&f["carried"]) != [0, 0] {
                found.carried += 1;
            }
            let expected: Counts = std::array::from_fn(|i| ITEMS[i].1);
            assert_eq!(
                total, expected,
                "kill {kill}: items duplicated or lost: {line}"
            );
            // The committed tick: the marker, the chest's name and where every item was.
            let tick = start - 1;
            if let Some(phase) = aimed {
                assert_eq!(
                    tick % CYCLE_TICKS,
                    phase,
                    "kill {kill}: not the aimed commit: {line}"
                );
            }
            let p = path(tick);
            assert_eq!(
                f["golds"],
                format!("{},{}", p.x, p.z),
                "kill {kill}: {line}"
            );
            assert_eq!(f["name"], format!("t{tick}"), "kill {kill}: {line}");
            assert_eq!(
                ticks.get(&tick),
                Some(&in_places),
                "kill {kill}: the commit of tick {tick} holds another tick's items: {line}"
            );
        }
        if start > last_start {
            found.advanced += 1;
            last_start = start;
        }
        if mode == MODE_RUN {
            let after = afters[usize::try_from(kill).unwrap()];
            kill_after_commit(&mut server, kill, after, &mut ticks);
        }
        server.child.wait().unwrap();
        for line in &server.lines {
            record(&line, &mut ticks);
        }
        aimed = aim;
    }
    remove(&data);
    found
}

#[test]
fn forced_kills_never_duplicate_lose_or_split_items() {
    // A fixed-seed generator for the kill moments, in kill order.
    let mut rng = Rng(0x17e5_c0de);
    let afters: Vec<u64> = (0..KILLS).map(|_| rng.next() % 30).collect();
    // The kills run in two chains side by side, each on a server file of its own, at the moments
    // and aims one chain of all of them takes: the children tick in real time, so one chain
    // takes twice as long for the same kills.
    let half = KILLS / 2;
    let chains = side_by_side(&[(0, half), (half, KILLS)], |&(first, end)| {
        chain(first, end, &afters)
    });
    let advanced: u32 = chains.iter().map(|c| c.advanced).sum();
    assert_eq!(advanced, KILLS, "every restart finds a newer commit");
    // The commits caught items in every place (player, chest, ground) many times: the aimed
    // ones alone catch the ground ten times and the cursor ten times.
    let held: [u32; 3] = std::array::from_fn(|i| chains.iter().map(|c| c.held[i]).sum());
    assert!(
        held.iter().all(|&n| n >= 10),
        "items found by place: {held:?}"
    );
    let carried: u32 = chains.iter().map(|c| c.carried).sum();
    assert!(
        carried >= 10,
        "only {carried} commits held items on the cursor"
    );
}

/// Takes a commit at the next tick boundary: the items its player data and block entities hold.
fn commit_counts(state: &mut State, output: &mut Output) -> Counts {
    state.saving.requested = true;
    let pool = crate::runtime::cpu_pool(1).unwrap();
    let mut log = InputLog::default();
    let mut answers = std::mem::take(&mut output.answers);
    crate::tick::step(
        state,
        &mut Vec::new(),
        &mut answers,
        &pool,
        output,
        &mut log,
    );
    deliver(state, output, &mut log);
    boundary(state, output, &mut log);
    assert!(pool.shutdown());
    let snapshot = output
        .requests
        .drain(..)
        .find_map(|r| match r {
            crate::save::Request::Commit(s) => Some(s),
            _ => None,
        })
        .expect("a snapshot");
    let mut counts = [0; 2];
    for (_, data) in &snapshot.players {
        for value in data.extra.values() {
            if let Value::List(List::Compound(list)) = value {
                for s in stacks(list) {
                    add(&mut counts, &s);
                }
            }
        }
    }
    for (_, _, kind, list) in &snapshot.extras {
        if *kind == ChunkKind::BlockEntities {
            for c in list.iter() {
                if let Some(Value::List(List::Compound(items))) = c.get("Items") {
                    for s in stacks(items) {
                        add(&mut counts, &s);
                    }
                }
            }
        }
    }
    counts
}

/// A commit taken while a player holds items on its cursor (taken out of a chest through its
/// window) still holds those items: the chest's saved list no longer has them, so the player's
/// saved data must, or a crash right after the commit loses them.
#[test]
fn a_commit_holds_the_items_on_a_cursor() {
    let mut state = new_state(settings(|_| {}), Listeners::default());
    state.saving.players = true;
    let mut output = Output::default();
    let (j, mut rx) = join(1, 0);
    tick(&mut state, &mut vec![j, radius(1, 1, 4)], &mut output);
    for _ in 0..4 {
        tick(&mut state, &mut Vec::new(), &mut output);
    }
    let runtime_id = state.players[&SESSION].runtime_id;
    let init: Input = initialised(1, 2, runtime_id);
    tick(&mut state, &mut vec![init], &mut output);
    // A chest with five sticks, open.
    let at = BlockPos::new(2, -60, 1);
    state.worlds[0]
        .set_block(at, BlockKind::Chest.default_state())
        .unwrap();
    block_entity::placed(&mut state, 0, at, None, &Stack::EMPTY, Face::Up);
    state.rules.block_entities.get_mut(0, at).unwrap().items[0] = Stack::new(item(1), 5);
    assert!(inventory::open_block(&mut state, SESSION, 0, at));
    tick(&mut state, &mut Vec::new(), &mut output);
    let packets = received(&mut rx).0;
    let window = packets
        .iter()
        .find_map(|p| match p {
            Packet::ContainerOpen(o) => Some(o.window_id),
            _ => None,
        })
        .expect("the chest opened");
    let net = packets
        .iter()
        .find_map(|p| match p {
            Packet::InventoryContent(c) if c.window_id == u32::from(window) => {
                Some(c.content[0].stack_network_id)
            }
            _ => None,
        })
        .expect("the chest's slots were sent");
    let slot = |container: u8, slot: u8, stack_network_id: i32| StackRequestSlotInfo {
        container: FullContainerName {
            container_id: container,
            dynamic_container_id: None,
        },
        slot,
        stack_network_id,
    };
    // The five sticks go onto the cursor.
    let take = ItemStackRequest {
        request_id: -1,
        actions: vec![StackRequestAction::Take(TakeStackRequestAction {
            transfer_stack_request_action: TransferStackRequestAction {
                count: 5,
                source: slot(7, 0, net),
                destination: slot(59, 0, 0),
            },
        })],
        filter_strings: Vec::new(),
        filter_cause: 0,
    };
    let request = input(
        1,
        3,
        packet(Packet::ItemStackRequest(RequestPacket {
            requests: vec![take],
        })),
    );
    tick(&mut state, &mut vec![request], &mut output);
    let done = received(&mut rx).0.iter().any(
        |p| matches!(p, Packet::ItemStackResponse(r) if r.responses.iter().all(|r| r.status == 0)),
    );
    assert!(done, "the take was done");
    assert_eq!(chest_counts(&state), [0, 0]);
    let sticks = commit_counts(&mut state, &mut output);
    assert_eq!(
        sticks[1], 5,
        "the commit holds the five sticks on the cursor (a crash after it would lose them)"
    );
}
