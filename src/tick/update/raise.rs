//! Raising the block events of the update rules (PMMP `BlockEventHelper` and the events PMMP's
//! block classes raise themselves): a rule that passed its check asks the listeners before it
//! changes blocks, and tells the monitors after.
//!
//! The check a rule makes again after the listeners is that no listener changed a block the
//! change reads or writes: the blocks at the positions it names are still those of the event.

use std::collections::BTreeMap;

use super::Cx;
use crate::block::State;
use crate::event::{
    BlockDeath, BlockForm, BlockGrow, BlockMelt, BlockSpread, Event, StructureGrow,
};
use crate::tick::dispatch;
use crate::world::{self, BlockPos};

/// A change the listeners refused: one cancelled it, or changed a block it depends on.
#[derive(Debug)]
pub(super) struct Refused;

impl Cx<'_> {
    /// The id of the rule's world, for events.
    pub(super) fn world_id(&self) -> world::Id {
        world::Id::of(self.world)
    }

    /// Asks the listeners of `E` whether the change may happen; a listener that changes a block
    /// at `at` refuses it. `Ok(None)`: nobody listens or watches, and no event was built.
    /// `Ok(Some(event))`: the event goes to [`Self::tell`] once the change is applied.
    pub(super) fn ask<E: Event, const N: usize>(
        &mut self,
        at: [BlockPos; N],
        build: impl FnOnce(&Self) -> E,
        cancelled: fn(&E) -> bool,
    ) -> Result<Option<E>, Refused> {
        if !dispatch::heard::<E>(self.state) {
            return Ok(None);
        }
        let event = build(self);
        self.call(event, cancelled, |cx| at.map(|p| cx.get(p)))
            .map(Some)
    }

    /// Calls the monitors of an applied change, if its event was built.
    pub(super) fn tell<E: Event>(&mut self, event: Option<E>) {
        if let Some(event) = event {
            dispatch::watch(self.state, &event);
        }
    }

    /// Grows the block at `pos` into `new` (PMMP `BlockEventHelper::grow`) from the plant at
    /// `parent` (`pos` again for a plant that grows in place), by bone meal of the rule's player
    /// if one used it. Whether the block was set.
    pub(super) fn grow(&mut self, pos: BlockPos, new: State, parent: BlockPos) -> bool {
        let player = self.player;
        self.change(
            pos,
            new,
            parent,
            |world, block| BlockGrow {
                world,
                pos,
                block,
                new_state: new,
                player,
                cancelled: false,
            },
            BlockGrow::is_cancelled,
        )
    }

    /// Spreads the block at `source` to `pos` (PMMP `BlockEventHelper::spread`). Whether the
    /// block was set.
    pub(super) fn spread(&mut self, pos: BlockPos, new: State, source: BlockPos) -> bool {
        self.change(
            pos,
            new,
            source,
            |world, block| BlockSpread {
                world,
                pos,
                block,
                new_state: new,
                source,
                cancelled: false,
            },
            BlockSpread::is_cancelled,
        )
    }

    /// Forms the block at `pos` into `new` by the block at `cause` (PMMP
    /// `BlockEventHelper::form`): the water or lava, or the blue ice that makes basalt. Whether
    /// the block was set.
    pub(super) fn form(&mut self, pos: BlockPos, new: State, cause: BlockPos) -> bool {
        self.change(
            pos,
            new,
            cause,
            |world, block| BlockForm {
                world,
                pos,
                block,
                new_state: new,
                causing_block: cause,
                cancelled: false,
            },
            BlockForm::is_cancelled,
        )
    }

    /// Melts the block at `pos` into `new` (PMMP `BlockEventHelper::melt`). Whether the block
    /// was set.
    pub(super) fn melt(&mut self, pos: BlockPos, new: State) -> bool {
        self.change(
            pos,
            new,
            pos,
            |world, block| BlockMelt {
                world,
                pos,
                block,
                new_state: new,
                cancelled: false,
            },
            BlockMelt::is_cancelled,
        )
    }

    /// Turns the block at `pos` into its dead form `new` (PMMP `BlockEventHelper::die`). Whether
    /// the block was set.
    pub(super) fn die(&mut self, pos: BlockPos, new: State) -> bool {
        self.change(
            pos,
            new,
            pos,
            |world, block| BlockDeath {
                world,
                pos,
                block,
                new_state: new,
                cancelled: false,
            },
            BlockDeath::is_cancelled,
        )
    }

    /// Asks the listeners of [`StructureGrow`] whether the block at `origin` may set `blocks`,
    /// as [`Self::ask`] does; a listener that changes the growing block or any block of the
    /// structure refuses it.
    pub(super) fn ask_structure(
        &mut self,
        origin: BlockPos,
        blocks: &BTreeMap<BlockPos, State>,
    ) -> Result<Option<StructureGrow>, Refused> {
        if !dispatch::heard::<StructureGrow>(self.state) {
            return Ok(None);
        }
        let block = self.get(origin).ok_or(Refused)?;
        let event = StructureGrow {
            world: self.world_id(),
            pos: origin,
            block,
            blocks: blocks.iter().map(|(&p, &s)| (p, s)).collect(),
            player: self.player,
            cancelled: false,
        };
        self.call(event, StructureGrow::is_cancelled, |cx| {
            std::iter::once(origin)
                .chain(blocks.keys().copied())
                .map(|p| cx.get(p))
                .collect::<Vec<_>>()
        })
        .map(Some)
    }

    /// Sets the block at `pos` to `new` once the listeners of `E` let it; `other` is a second
    /// block the change depends on (its cause), or `pos` again. Whether the block was set.
    fn change<E: Event>(
        &mut self,
        pos: BlockPos,
        new: State,
        other: BlockPos,
        build: impl FnOnce(world::Id, State) -> E,
        cancelled: fn(&E) -> bool,
    ) -> bool {
        if !dispatch::heard::<E>(self.state) {
            return self.set(pos, new);
        }
        let Some(old) = self.get(pos) else {
            return false;
        };
        let Ok(event) = self.ask([pos, other], |cx| build(cx.world_id(), old), cancelled) else {
            return false;
        };
        let done = self.set(pos, new);
        if done {
            self.tell(event);
        }
        done
    }

    /// Calls the listeners of `event`, then checks again the blocks `read` reads: a cancel, or a
    /// listener that changed one of them, refuses the change. The blocks are read only when a
    /// listener, not only a monitor, is registered.
    fn call<E: Event, B: PartialEq>(
        &mut self,
        mut event: E,
        cancelled: fn(&E) -> bool,
        read: impl Fn(&Self) -> B,
    ) -> Result<E, Refused> {
        let before = dispatch::listened::<E>(self.state).then(|| read(self));
        dispatch::call(self.state, &mut event);
        if cancelled(&event) || before.is_some_and(|b| read(self) != b) {
            return Err(Refused);
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{self, Face, Kind, PropertyValue};
    use crate::event::{
        Actor, BlockBurn, BlockUpdate, EntityTrampleFarmland, FarmlandHydrationChange, LeavesDecay,
        Listeners, Priority,
    };
    use crate::player;
    use crate::tick::tests::{new_state, settings};
    use crate::tick::update::{
        Effect, fertilize, fire, on_random, on_scheduled, props, tick, trample,
    };
    use crate::tick::{Context, SessionId, State as Server};

    /// A test server (light off: every chunk lit, at noon) whose listeners `add` registers.
    fn server(add: impl FnOnce(&mut Listeners)) -> Server {
        let mut listeners = Listeners::default();
        add(&mut listeners);
        let mut state = new_state(
            settings(|c| c.developer.lighting = crate::config::Lighting::Fixed),
            listeners,
        );
        state.worlds[0].set_time(6000);
        state
    }

    /// A bounded channel a listener records into.
    fn record<T: Send + 'static>() -> (tokio::sync::mpsc::Sender<T>, tokio::sync::mpsc::Receiver<T>)
    {
        tokio::sync::mpsc::channel(1 << 16)
    }

    fn drain<T>(rx: &mut tokio::sync::mpsc::Receiver<T>) -> Vec<T> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn set(state: &mut Server, pos: BlockPos, s: State) {
        state.worlds[0].set_block(pos, s).unwrap();
    }

    fn get(state: &Server, pos: BlockPos) -> State {
        state.worlds[0].block(pos).unwrap()
    }

    fn named(name: &str) -> State {
        State::all().find(|s| s.name() == name).unwrap()
    }

    fn find(name: &str, props: &[(&str, PropertyValue<'_>)]) -> State {
        State::find(name, props).unwrap()
    }

    fn farmland(wetness: i32) -> State {
        find(
            "minecraft:farmland",
            &[("moisturized_amount", PropertyValue::Int(wetness))],
        )
    }

    fn someone() -> player::Id {
        player::Id::new(SessionId::new(7))
    }

    fn stone() -> State {
        Kind::Stone.default_state()
    }

    /// What a test listener does.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Do {
        Nothing,
        Cancel,
        /// Puts stone where the event's block is: a listener that breaks the rule's check.
        Replace,
    }

    // pmmp: BlockGrowEvent
    #[test]
    fn bone_meal_raises_block_grow_naming_the_player() {
        let at = BlockPos::new(5, -60, 5);
        let wheat = named("minecraft:wheat");
        let run = |behaviour: Do| {
            let (tx, mut rx) = record();
            let (seen_tx, mut seen) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockGrow, cx: &mut Context<'_>| {
                        tx.try_send((e.world(), e.pos(), e.block(), e.new_state(), e.player()))
                            .unwrap();
                        match behaviour {
                            Do::Nothing => {}
                            Do::Cancel => e.cancel(),
                            Do::Replace => cx.state.worlds[0].set_block(e.pos(), stone()).unwrap(),
                        }
                    },
                );
                l.push_monitor(move |e: &BlockGrow, cx: &Context<'_>| {
                    let now = cx.state.worlds[0].block(e.pos());
                    seen_tx.try_send(now == Some(e.new_state())).unwrap();
                });
            });
            set(&mut state, at.offset(0, -1, 0).unwrap(), farmland(7));
            set(&mut state, at, wheat);
            state.worlds[0].take_changes();
            let used = fertilize(&mut state, 0, at, Some(someone()));
            let changes = state.worlds[0].take_changes();
            (
                used,
                get(&state, at),
                drain(&mut rx),
                drain(&mut seen),
                changes,
            )
        };
        let (used, grown, calls, seen, _) = run(Do::Nothing);
        assert!(used);
        assert_ne!(grown, wheat);
        assert_eq!(
            calls,
            [(world::Id::DEFAULT, at, wheat, grown, Some(someone()))]
        );
        assert_eq!(seen, [true], "the monitor sees the grown crop");
        let (used, kept, calls, seen, changes) = run(Do::Cancel);
        assert!(!used, "a cancelled growth keeps the bone meal");
        assert_eq!(kept, wheat);
        assert_eq!(calls.len(), 1);
        assert!(seen.is_empty(), "no monitor for a cancelled growth");
        assert!(changes.is_empty());
        let (used, replaced, _, seen, _) = run(Do::Replace);
        assert!(!used);
        assert_eq!(replaced, stone(), "the listener's block stays");
        assert!(seen.is_empty());
    }

    /// A stem whose `BlockGrow` listener replaces it while its fruit grows keeps the listener's
    /// block, and no fruit grows.
    // pmmp: BlockGrowEvent
    #[test]
    fn a_stem_a_growth_listener_replaced_stays_replaced() {
        let at = BlockPos::new(5, -60, 5);
        let (tx, mut rx) = record();
        let mut state = server(|l| {
            l.push(
                Priority::Normal,
                move |e: &mut BlockGrow, cx: &mut Context<'_>| {
                    tx.try_send(e.new_state().kind()).unwrap();
                    cx.state.worlds[0].set_block(at, stone()).unwrap();
                },
            );
        });
        let grown = props::with_int(named("minecraft:melon_stem"), "growth", 7).unwrap();
        let stem = props::with_facing(grown, Face::Up).unwrap();
        set(&mut state, at.offset(0, -1, 0).unwrap(), farmland(7));
        set(&mut state, at, stem);
        let mut calls = Vec::new();
        for _ in 0..2000 {
            on_random(&mut Cx::new(&mut state, 0, at), at, stem);
            calls = drain(&mut rx);
            if !calls.is_empty() {
                break;
            }
        }
        assert_eq!(calls, [Kind::Melon]);
        assert_eq!(get(&state, at), stone(), "the listener's block stays");
        for face in Face::HORIZONTAL {
            assert_eq!(get(&state, at.step(face).unwrap()), block::AIR, "no melon");
        }
    }

    /// Sugar cane whose `BlockGrow` listener replaces its bottom block keeps the listener's
    /// block, and no cane grows.
    // pmmp: BlockGrowEvent
    #[test]
    fn sugar_cane_keeps_the_bottom_a_growth_listener_replaced() {
        let bottom = BlockPos::new(5, -60, 5);
        let above = bottom.offset(0, 1, 0).unwrap();
        let (tx, mut rx) = record();
        let mut state = server(|l| {
            l.push(
                Priority::Normal,
                move |e: &mut BlockGrow, cx: &mut Context<'_>| {
                    tx.try_send(e.pos()).unwrap();
                    cx.state.worlds[0].set_block(bottom, stone()).unwrap();
                },
            );
        });
        set(&mut state, bottom, Kind::Sugarcane.default_state());
        assert!(!fertilize(&mut state, 0, bottom, Some(someone())));
        assert_eq!(drain(&mut rx), [above]);
        assert_eq!(get(&state, bottom), stone(), "the listener's block stays");
        assert_eq!(get(&state, above), block::AIR);
    }

    /// Ready bamboo whose `StructureGrow` listener replaces it keeps the listener's block.
    // pmmp: StructureGrowEvent
    #[test]
    fn bamboo_a_structure_listener_replaced_stays_replaced() {
        let at = BlockPos::new(5, -60, 5);
        let (tx, mut rx) = record();
        let mut state = server(|l| {
            l.push(
                Priority::Normal,
                move |e: &mut StructureGrow, cx: &mut Context<'_>| {
                    tx.try_send(e.pos()).unwrap();
                    cx.state.worlds[0].set_block(at, stone()).unwrap();
                },
            );
        });
        let ready = props::with_flag(Kind::Bamboo.default_state(), "age_bit", true).unwrap();
        set(&mut state, at, ready);
        on_random(&mut Cx::new(&mut state, 0, at), at, ready);
        assert_eq!(drain(&mut rx), [at]);
        assert_eq!(get(&state, at), stone(), "the listener's block stays");
    }

    // pmmp: StructureGrowEvent
    #[test]
    fn saplings_raise_structure_grow_with_every_block() {
        let at = BlockPos::new(20, -60, 20);
        let sapling = named("minecraft:oak_sapling");
        let run = |behaviour: Do| {
            let (tx, mut rx) = record();
            let (seen_tx, mut seen) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut StructureGrow, cx: &mut Context<'_>| {
                        tx.try_send((e.pos(), e.block(), e.blocks().to_vec(), e.player()))
                            .unwrap();
                        match behaviour {
                            Do::Nothing => {}
                            Do::Cancel => e.cancel(),
                            Do::Replace => {
                                let (p, _) = e.blocks()[e.blocks().len() / 2];
                                cx.state.worlds[0].set_block(p, stone()).unwrap();
                            }
                        }
                    },
                );
                l.push_monitor(move |e: &StructureGrow, cx: &Context<'_>| {
                    let w = &cx.state.worlds[0];
                    let applied = e.blocks().iter().all(|&(p, s)| w.block(p) == Some(s));
                    seen_tx.try_send(applied).unwrap();
                });
            });
            set(&mut state, at, sapling);
            state.worlds[0].take_changes();
            state.tick = 1234;
            let used = fertilize(&mut state, 0, at, Some(someone()));
            let changes = state.worlds[0].take_changes();
            (used, changes, drain(&mut rx), drain(&mut seen))
        };
        let (used, changes, calls, seen) = run(Do::Nothing);
        assert!(used);
        let [(pos, block, blocks, player)] = calls.as_slice() else {
            panic!("{} calls", calls.len());
        };
        assert_eq!((*pos, *block, *player), (at, sapling, Some(someone())));
        assert!(blocks.iter().any(|(_, s)| s.kind() == Kind::OakLog));
        assert!(changes.keys().all(|p| blocks.iter().any(|(q, _)| q == p)));
        assert_eq!(seen, [true]);
        let (used, changes, _, seen) = run(Do::Cancel);
        assert!(!used);
        assert!(changes.is_empty(), "a cancelled tree changes nothing");
        assert!(seen.is_empty());
        let (used, changes, calls, seen) = run(Do::Replace);
        assert!(!used);
        let (p, _) = calls[0].2[calls[0].2.len() / 2];
        assert_eq!(changes.len(), 1, "only the listener's block changed");
        assert_eq!(changes.get(&p), Some(&crate::world::Entry::Known(stone())));
        assert!(seen.is_empty());
    }

    fn advance(state: &mut Server, ticks: u32) {
        for _ in 0..ticks {
            tick(state);
            state.tick += 1;
        }
    }

    // pmmp: BlockSpreadEvent
    #[test]
    fn liquid_flow_raises_block_spread_and_cancelling_stops_it() {
        let source = BlockPos::new(8, -60, 8);
        let run = |behaviour: Do| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockSpread, cx: &mut Context<'_>| {
                        let from = cx.state.worlds[0].block(e.source());
                        tx.try_send((e.pos(), e.block(), e.new_state(), e.source(), from))
                            .unwrap();
                        if behaviour == Do::Cancel {
                            e.cancel();
                        }
                    },
                );
                l.push_monitor(|e: &BlockSpread, cx: &Context<'_>| {
                    assert_eq!(cx.state.worlds[0].block(e.pos()), Some(e.new_state()));
                });
            });
            set(&mut state, source, Kind::Water.default_state());
            advance(&mut state, 60);
            let water = (-2..=18)
                .flat_map(|x| (-2..=18).map(move |z| BlockPos::new(x, -60, z)))
                .filter(|p| get(&state, *p).kind() == Kind::Water)
                .count();
            (water, drain(&mut rx))
        };
        let (spread, calls) = run(Do::Nothing);
        assert!(spread > 20, "{spread}");
        assert_eq!(
            calls.len(),
            spread - 1,
            "one event per block the water reached"
        );
        for (pos, block, new, from, from_block) in calls {
            assert_eq!(block, block::AIR);
            assert_eq!(new.kind(), Kind::Water);
            assert_eq!(from_block.map(State::kind), Some(Kind::Water));
            let d = (pos.x - from.x).abs() + (pos.y - from.y).abs() + (pos.z - from.z).abs();
            assert_eq!(d, 1, "{pos:?} from {from:?}");
        }
        let (spread, calls) = run(Do::Cancel);
        assert_eq!(spread, 1, "only the source");
        assert!(!calls.is_empty());
    }

    // pmmp: BlockBurnEvent
    #[test]
    fn fire_raises_block_burn_and_cancelling_keeps_the_fuel() {
        let planks = Kind::OakPlanks.default_state();
        let run = |cancel: bool| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockBurn, cx: &mut Context<'_>| {
                        let fire = cx.state.worlds[0].block(e.causing_block());
                        tx.try_send((e.block(), fire.map(State::kind))).unwrap();
                        if cancel {
                            e.cancel();
                        }
                    },
                );
            });
            for x in 0..5 {
                for z in 0..5 {
                    set(&mut state, BlockPos::new(x, -60, z), planks);
                }
            }
            set(
                &mut state,
                BlockPos::new(2, -59, 2),
                Kind::Fire.default_state(),
            );
            advance(&mut state, 3000);
            let left = (0..5)
                .flat_map(|x| (0..5).map(move |z| BlockPos::new(x, -60, z)))
                .filter(|p| get(&state, *p) == planks)
                .count();
            (left, drain(&mut rx))
        };
        let (left, calls) = run(true);
        assert_eq!(left, 25, "cancelled burns keep every plank");
        assert!(!calls.is_empty());
        assert!(
            calls.iter().all(|c| *c == (planks, Some(Kind::Fire))),
            "{calls:?}"
        );
        let (left, calls) = run(false);
        assert!(left < 25 && !calls.is_empty());
    }

    /// A fire that a `BlockBurn` listener puts out sets nothing else alight in that update.
    // pmmp: BlockBurnEvent
    #[test]
    fn a_fire_a_burn_listener_put_out_spreads_no_further() {
        let at = BlockPos::new(8, -60, 8);
        let (tx, mut rx) = record();
        let mut state = server(|l| {
            l.push(
                Priority::Normal,
                move |e: &mut BlockBurn, cx: &mut Context<'_>| {
                    cx.state.worlds[0]
                        .set_block(e.causing_block(), block::AIR)
                        .unwrap();
                    e.cancel();
                    tx.try_send(()).unwrap();
                },
            );
        });
        let planks = Kind::OakPlanks.default_state();
        let fire = Kind::Fire.default_state();
        set(&mut state, at.step(Face::Down).unwrap(), planks);
        for face in Face::HORIZONTAL {
            set(&mut state, at.step(face).unwrap(), planks);
        }
        let others = |state: &Server| {
            (7..=9)
                .flat_map(|x| (-60..=-55).flat_map(move |y| (7..=9).map(move |z| (x, y, z))))
                .map(|(x, y, z)| BlockPos::new(x, y, z))
                .filter(|&p| p != at && get(state, p).kind() == Kind::Fire)
                .collect::<Vec<_>>()
        };
        let (mut put_out, mut lit_after) = (0, 0);
        for _ in 0..300 {
            for p in others(&state) {
                set(&mut state, p, block::AIR);
            }
            set(&mut state, at, fire);
            fire::on_random(&mut Cx::new(&mut state, 0, at), at, fire);
            if !drain(&mut rx).is_empty() {
                put_out += 1;
                lit_after += others(&state).len();
            }
        }
        assert!(put_out > 0, "no burn reached the listener");
        assert_eq!(lit_after, 0, "a fire put out by a listener spread");
    }

    // pmmp: BlockFormEvent
    #[test]
    fn concrete_and_lava_raise_block_form_naming_the_cause() {
        let powder = named("minecraft:red_concrete_powder");
        let lava = Kind::Lava.default_state();
        let (p, l) = (BlockPos::new(3, -60, 3), BlockPos::new(10, -60, 10));
        let (p_water, l_water) = (BlockPos::new(4, -60, 3), BlockPos::new(10, -59, 10));
        let run = |cancel: bool| {
            let (tx, mut rx) = record();
            let mut state = server(|listeners| {
                listeners.push(
                    Priority::Normal,
                    move |e: &mut BlockForm, _: &mut Context<'_>| {
                        tx.try_send((e.pos(), e.block(), e.new_state(), e.causing_block()))
                            .unwrap();
                        if cancel {
                            e.cancel();
                        }
                    },
                );
            });
            set(&mut state, BlockPos::new(3, -61, 3), stone());
            set(&mut state, p, powder);
            set(&mut state, p_water, Kind::Water.default_state());
            set(&mut state, l, lava);
            set(&mut state, l_water, Kind::Water.default_state());
            advance(&mut state, 2);
            ((get(&state, p), get(&state, l)), drain(&mut rx))
        };
        let ((concrete, obsidian), calls) = run(false);
        assert_eq!(concrete, named("minecraft:red_concrete"));
        assert_eq!(obsidian.kind(), Kind::Obsidian);
        assert!(calls.contains(&(p, powder, concrete, p_water)), "{calls:?}");
        assert!(calls.contains(&(l, lava, obsidian, l_water)), "{calls:?}");
        let (kept, _) = run(true);
        assert_eq!(kept, (powder, lava));
    }

    // pmmp: BlockMeltEvent, BlockDeathEvent
    #[test]
    fn ice_melts_and_coral_dies_through_their_events() {
        let ice = find("minecraft:frosted_ice", &[("age", PropertyValue::Int(3))]);
        let coral = named("minecraft:tube_coral_block");
        let (i, c) = (BlockPos::new(2, -60, 2), BlockPos::new(9, -60, 9));
        let run = |cancel: bool| {
            let (melts, mut melted) = record();
            let (deaths, mut died) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockMelt, _: &mut Context<'_>| {
                        melts.try_send((e.pos(), e.block(), e.new_state())).unwrap();
                        if cancel {
                            e.cancel();
                        }
                    },
                );
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockDeath, _: &mut Context<'_>| {
                        deaths
                            .try_send((e.pos(), e.block(), e.new_state()))
                            .unwrap();
                        if cancel {
                            e.cancel();
                        }
                    },
                );
            });
            set(&mut state, i, ice);
            set(&mut state, c, coral);
            on_scheduled(&mut Cx::new(&mut state, 0, i), i, ice);
            on_scheduled(&mut Cx::new(&mut state, 0, c), c, coral);
            (
                (get(&state, i), get(&state, c)),
                drain(&mut melted),
                drain(&mut died),
            )
        };
        let ((water, dead), melted, died) = run(false);
        assert_eq!(water, Kind::Water.default_state());
        assert_eq!(dead, named("minecraft:dead_tube_coral_block"));
        assert_eq!(melted, [(i, ice, water)]);
        assert_eq!(died, [(c, coral, dead)]);
        let (kept, melted, died) = run(true);
        assert_eq!(kept, (ice, coral));
        assert_eq!((melted.len(), died.len()), (1, 1));
    }

    // pmmp: LeavesDecayEvent
    #[test]
    fn leaves_decay_through_their_event_and_cancelled_leaves_stop_checking() {
        let leaves = find(
            "minecraft:oak_leaves",
            &[
                ("persistent_bit", PropertyValue::Byte(0)),
                ("update_bit", PropertyValue::Byte(1)),
            ],
        );
        let at = BlockPos::new(4, -55, 4);
        let run = |behaviour: Do| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut LeavesDecay, cx: &mut Context<'_>| {
                        tx.try_send((e.pos(), e.block())).unwrap();
                        match behaviour {
                            Do::Nothing => {}
                            Do::Cancel => e.cancel(),
                            Do::Replace => {
                                cx.state.worlds[0].set_block(e.pos(), stone()).unwrap();
                            }
                        }
                    },
                );
            });
            set(&mut state, at, leaves);
            on_random(&mut Cx::new(&mut state, 0, at), at, leaves);
            let effects = std::mem::take(&mut state.rules.updates.effects);
            (get(&state, at), effects, drain(&mut rx))
        };
        let (_, effects, calls) = run(Do::Nothing);
        assert_eq!(effects, [Effect::Destroyed(at)]);
        assert_eq!(calls, [(at, leaves)]);
        let (kept, effects, _) = run(Do::Cancel);
        assert!(effects.is_empty());
        assert_eq!(kept.kind(), leaves.kind());
        assert_eq!(props::flag(kept, "update_bit"), Some(false));
        let (replaced, effects, _) = run(Do::Replace);
        assert!(effects.is_empty());
        assert_eq!(replaced, stone());
    }

    // pmmp: FarmlandHydrationChangeEvent
    #[test]
    fn farmland_hydration_change_sets_the_wetness() {
        let at = BlockPos::new(6, -61, 6);
        let run = |wetness: i32, set_to: Option<u8>, cancel: bool| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut FarmlandHydrationChange, _: &mut Context<'_>| {
                        tx.try_send((e.old_hydration(), e.new_hydration())).unwrap();
                        if let Some(v) = set_to {
                            e.set_new_hydration(v);
                        }
                        if cancel {
                            e.cancel();
                        }
                    },
                );
            });
            set(&mut state, at, farmland(wetness));
            on_random(&mut Cx::new(&mut state, 0, at), at, farmland(wetness));
            (get(&state, at), drain(&mut rx))
        };
        assert_eq!(run(7, None, false), (farmland(6), vec![(7, 6)]));
        assert_eq!(run(7, Some(3), false), (farmland(3), vec![(7, 6)]));
        assert_eq!(run(7, Some(200), false).0, farmland(7), "at most 7");
        assert_eq!(run(7, None, true).0, farmland(7));
        // Dry farmland becomes dirt without an event (PMMP).
        assert_eq!(run(0, None, false), (block::DIRT, Vec::new()));
    }

    // pmmp: BlockUpdateEvent
    #[test]
    fn cancelled_block_updates_skip_the_reaction() {
        let torch = find(
            "minecraft:torch",
            &[("torch_facing_direction", PropertyValue::String("west"))],
        );
        let (holder, at) = (BlockPos::new(9, -59, 5), BlockPos::new(10, -59, 5));
        let run = |cancel: bool| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut BlockUpdate, _: &mut Context<'_>| {
                        tx.try_send((e.pos(), e.block())).unwrap();
                        if cancel && e.pos() == at {
                            e.cancel();
                        }
                    },
                );
            });
            set(&mut state, holder, stone());
            set(&mut state, at, torch);
            advance(&mut state, 1);
            drain(&mut rx);
            set(&mut state, holder, block::AIR);
            advance(&mut state, 1);
            let effects = std::mem::take(&mut state.rules.updates.effects);
            (get(&state, at), effects, drain(&mut rx))
        };
        let (_, effects, calls) = run(false);
        assert_eq!(effects, [Effect::Destroyed(at)]);
        assert!(calls.contains(&(at, torch)));
        assert!(calls.contains(&(holder, block::AIR)));
        let (kept, effects, _) = run(true);
        assert_eq!(kept, torch, "the torch never heard its holder go");
        assert!(effects.is_empty());
    }

    // pmmp: EntityTrampleFarmlandEvent
    #[test]
    fn trampling_raises_entity_trample_farmland() {
        let at = BlockPos::new(2, -61, 2);
        let who = Actor::Player(someone());
        let run = |cancel: bool| {
            let (tx, mut rx) = record();
            let mut state = server(|l| {
                l.push(
                    Priority::Normal,
                    move |e: &mut EntityTrampleFarmland, _: &mut Context<'_>| {
                        tx.try_send((e.entity(), e.pos(), e.block())).unwrap();
                        if cancel {
                            e.cancel();
                        }
                    },
                );
            });
            set(&mut state, at, farmland(0));
            trample(&mut state, 0, at, 0.4, who);
            assert!(drain(&mut rx).is_empty(), "a soft landing tramples nothing");
            trample(&mut state, 0, at, 1.5, who);
            (get(&state, at), drain(&mut rx))
        };
        assert_eq!(run(false), (block::DIRT, vec![(who, at, farmland(0))]));
        assert_eq!(run(true).0, farmland(0));
    }
}
