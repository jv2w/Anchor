//! Scoreboards and boss bars in the tick: what each player was last sent, and the packets that
//! turn it into what the user sets now.
//!
//! Each slot has its own objective, `anchor:<slot>`. A text line is a fake player whose entry ID
//! is negative and fixed by slot and line; a player line is the player's own identity, its entry
//! ID and unique ID its runtime ID, so the same player is the same identity in every slot. A
//! changed line is removed and added again (the client does not rename an entry in place), all
//! removals in one `SetScore` before the additions in another.

use std::collections::BTreeSet;

use super::{Context, Phase, open};
use crate::display::{
    BossBar, Error, Holder, MAX_LINES, MAX_SIDEBAR_LINES, MAX_TEXT, Result, Scoreboard, Slot, Sort,
};
use crate::player;
use crate::protocol::consts::{BossEventType, ScoreboardIdentity};
use crate::protocol::packet::{BossEvent, Packet, RemoveObjective, SetDisplayObjective, SetScore};
use crate::protocol::{BOSS_EVENT_BOSS_BAR_TITLE_MAX_LEN, ScoreboardEntry};

/// Colour codes that fill empty text lines, so that each is its own blank line (dragonfly).
const BLANKS: [&str; 15] = [
    "§1", "§2", "§3", "§4", "§5", "§6", "§7", "§8", "§9", "§a", "§b", "§c", "§d", "§e", "§f",
];

/// What one player was last sent.
#[derive(Debug, Default)]
pub(super) struct Shown {
    /// The scoreboard of each slot, in [`Slot::ALL`] order.
    boards: [Option<Board>; 3],
    bar: Option<BossBar>,
}

/// A scoreboard as sent: its lines resolved to entries.
#[derive(Debug, Clone, PartialEq)]
struct Board {
    title: String,
    sort: Sort,
    entries: Vec<Entry>,
}

/// One line as sent.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: i64,
    who: Who,
    score: i32,
}

/// The identity of an entry.
#[derive(Debug, Clone, PartialEq)]
enum Who {
    /// A fake player showing this text.
    Text(String),
    /// A player, by unique ID.
    Player(i64),
}

crate::tick::checkpoint::fields!(Shown { boards, bar });
crate::tick::checkpoint::fields!(Board {
    title,
    sort,
    entries
});
crate::tick::checkpoint::fields!(Entry { id, who, score });
crate::tick::checkpoint::variants!(Who {
    0 => Text(text),
    1 => Player(unique_id),
});

impl Shown {
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        for board in &self.boards {
            h.u64(u64::from(board.is_some()));
            if let Some(b) = board {
                h.bytes(b.title.as_bytes());
                h.u64(u64::from(i32::from(b.sort).cast_unsigned()));
                h.usize(b.entries.len());
                for e in &b.entries {
                    h.u64(e.id.cast_unsigned());
                    h.u64(u64::from(e.score.cast_unsigned()));
                    match &e.who {
                        Who::Text(t) => h.bytes(t.as_bytes()),
                        Who::Player(u) => h.u64(u.cast_unsigned()),
                    }
                }
            }
        }
        h.u64(u64::from(self.bar.is_some()));
        if let Some(bar) = &self.bar {
            h.bytes(bar.title.as_bytes());
            h.u64(u64::from(bar.progress.to_bits()));
            h.u64(u64::from(u8::from(bar.colour)));
        }
    }
}

/// The position of `slot` in [`Slot::ALL`].
fn slot_index(slot: Slot) -> usize {
    match slot {
        Slot::List => 0,
        Slot::Sidebar => 1,
        Slot::BelowName => 2,
    }
}

/// The objective of `slot`.
fn objective(slot: Slot) -> String {
    format!("anchor:{}", slot.as_str())
}

/// A unique ID of an entity: Anchor's are the runtime IDs.
fn unique_id(runtime_id: u64) -> i64 {
    i64::try_from(runtime_id).unwrap_or(i64::MAX)
}

/// The `SetScore` entry of `objective` that adds or removes `e`.
fn score_entry(objective: &str, e: &Entry, change: Change) -> ScoreboardEntry {
    let (identity, entity, name) = match (change, &e.who) {
        (Change::Remove, _) => (ScoreboardIdentity::Remove, 0, String::new()),
        (Change::Add, Who::Text(t)) => (ScoreboardIdentity::FakePlayer, 0, t.clone()),
        (Change::Add, &Who::Player(u)) => (ScoreboardIdentity::Player, u, String::new()),
    };
    ScoreboardEntry {
        entry_id: e.id,
        objective_name: objective.to_owned(),
        score: e.score,
        identity_type: identity.into(),
        entity_unique_id: entity,
        display_name: name,
    }
}

/// Whether a `SetScore` entry adds or removes.
#[derive(Debug, Clone, Copy)]
enum Change {
    Add,
    Remove,
}

impl Context<'_> {
    /// Shows `board` to `player` in its slot, in place of what the slot showed. Only the lines
    /// that changed are sent again; a new title or sort order sends the whole scoreboard.
    ///
    /// Fails, changing nothing, if the player is not in the game, the scoreboard has more lines
    /// than its slot shows (15 in the sidebar, 1,024 elsewhere), a text is longer than 1,024
    /// bytes, or a player line names a player not in the game or one named before.
    pub fn set_scoreboard(&mut self, player: player::Id, board: Scoreboard) -> Result<()> {
        let max = if board.slot == Slot::Sidebar {
            MAX_SIDEBAR_LINES
        } else {
            MAX_LINES
        };
        if board.lines.len() > max {
            return Err(Error::TooManyLines);
        }
        if board.title.len() > MAX_TEXT {
            return Err(Error::TooLong);
        }
        let index = slot_index(board.slot);
        let base = i64::try_from(index * MAX_LINES).unwrap_or(i64::MAX);
        let mut players = BTreeSet::new();
        let mut entries = Vec::with_capacity(board.lines.len());
        for (i, line) in (0_i64..).zip(board.lines) {
            let (id, who) = match line.holder {
                Holder::Text(text) if text.len() > MAX_TEXT => return Err(Error::TooLong),
                Holder::Text(text) => {
                    let text = if text.is_empty() {
                        let blank = usize::try_from(i).unwrap_or(0) % BLANKS.len();
                        BLANKS.get(blank).copied().unwrap_or_default().to_owned()
                    } else {
                        text
                    };
                    (-1 - base - i, Who::Text(text))
                }
                Holder::Player(p) => {
                    let target = self
                        .state
                        .players
                        .get(&p.session())
                        .filter(|t| t.phase != Phase::Loading)
                        .ok_or(Error::UnknownPlayer)?;
                    let unique = unique_id(target.runtime_id);
                    if !players.insert(unique) {
                        return Err(Error::DuplicatePlayer);
                    }
                    (unique, Who::Player(unique))
                }
            };
            entries.push(Entry {
                id,
                who,
                score: line.score,
            });
        }
        let new = Board {
            title: board.title,
            sort: board.sort,
            entries,
        };
        let (p, open) = open(self.state, player).ok_or(Error::Offline)?;
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: slot_index is below the three slots"
        )]
        let shown = &mut open.display.boards[index];
        let name = objective(board.slot);
        let (removed, added): (Vec<&Entry>, Vec<&Entry>) = match shown.as_ref() {
            Some(old) if old.title == new.title && old.sort == new.sort => {
                let removed = (0..old.entries.len())
                    .filter(|&i| old.entries.get(i) != new.entries.get(i))
                    .filter_map(|i| old.entries.get(i))
                    .collect();
                let added = (0..new.entries.len())
                    .filter(|&i| old.entries.get(i) != new.entries.get(i))
                    .filter_map(|i| new.entries.get(i))
                    .collect();
                (removed, added)
            }
            old => {
                if old.is_some() {
                    p.send_packet(Packet::RemoveObjective(RemoveObjective {
                        objective_name: name.clone(),
                    }));
                }
                p.send_packet(Packet::SetDisplayObjective(SetDisplayObjective {
                    display_slot: board.slot.as_str().to_owned(),
                    objective_name: name.clone(),
                    display_name: new.title.clone(),
                    criteria_name: "dummy".to_owned(),
                    sort_order: new.sort.into(),
                }));
                (Vec::new(), new.entries.iter().collect())
            }
        };
        for (entries, change) in [(removed, Change::Remove), (added, Change::Add)] {
            if !entries.is_empty() {
                let entries = entries
                    .into_iter()
                    .map(|e| score_entry(&name, e, change))
                    .collect();
                p.send_packet(Packet::SetScore(SetScore { entries }));
            }
        }
        *shown = Some(new);
        Ok(())
    }

    /// Removes the scoreboard `player` sees in `slot`, if any. False if the player is not in the
    /// game.
    pub fn remove_scoreboard(&mut self, player: player::Id, slot: Slot) -> bool {
        let Some((p, open)) = open(self.state, player) else {
            return false;
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: slot_index is below the three slots"
        )]
        if open.display.boards[slot_index(slot)].take().is_some() {
            p.send_packet(Packet::RemoveObjective(RemoveObjective {
                objective_name: objective(slot),
            }));
        }
        true
    }

    /// Shows `bar` to `player` in place of the boss bar it saw. Only what changed is sent: a new
    /// title or progress updates the bar, a new colour shows it again.
    ///
    /// Fails, changing nothing, if the player is not in the game or the title is longer than
    /// 256 bytes.
    pub fn set_boss_bar(&mut self, player: player::Id, bar: BossBar) -> Result<()> {
        if bar.title.len() > BOSS_EVENT_BOSS_BAR_TITLE_MAX_LEN {
            return Err(Error::TooLong);
        }
        let (p, open) = open(self.state, player).ok_or(Error::Offline)?;
        let entity = unique_id(p.runtime_id);
        let event = |kind: BossEventType, bar: &BossBar| {
            Packet::BossEvent(BossEvent {
                boss_entity_unique_id: entity,
                event_type: kind.into(),
                boss_bar_title: bar.title.clone(),
                filtered_boss_bar_title: String::new(),
                health_percentage: bar.progress,
                colour: bar.colour.into(),
                overlay: 0,
            })
        };
        match &open.display.bar {
            Some(old) if old.colour == bar.colour => {
                if old.title != bar.title {
                    p.send_packet(event(BossEventType::Title, &bar));
                }
                if old.progress.to_bits() != bar.progress.to_bits() {
                    p.send_packet(event(BossEventType::HealthPercentage, &bar));
                }
            }
            old => {
                if let Some(old) = old {
                    p.send_packet(event(BossEventType::Hide, old));
                }
                p.send_packet(event(BossEventType::Show, &bar));
            }
        }
        open.display.bar = Some(bar);
        Ok(())
    }

    /// Hides the boss bar `player` sees, if any. False if the player is not in the game.
    pub fn remove_boss_bar(&mut self, player: player::Id) -> bool {
        let Some((p, open)) = open(self.state, player) else {
            return false;
        };
        if let Some(old) = open.display.bar.take() {
            p.send_packet(Packet::BossEvent(BossEvent {
                boss_entity_unique_id: unique_id(p.runtime_id),
                event_type: BossEventType::Hide.into(),
                boss_bar_title: old.title,
                filtered_boss_bar_title: String::new(),
                health_percentage: old.progress,
                colour: old.colour.into(),
                overlay: 0,
            }));
        }
        true
    }
}
