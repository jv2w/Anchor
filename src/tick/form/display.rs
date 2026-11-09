//! Scoreboards and boss bars in the tick: what each player was last sent, and the packets that
//! turn it into what the user sets now.
//!
//! Each slot has its own objective, `anchor:<slot>`. A text line is a fake player whose entry ID
//! is negative and fixed by slot and line; a player line is the player's own identity, its entry
//! ID and unique ID its runtime ID, so the same player is the same identity in every slot. A
//! changed line is removed and added again (the client does not rename an entry in place), all
//! removals in one `SetScore` before the additions in another.

use std::collections::{BTreeMap, BTreeSet};

use super::{Context, open};
use crate::display::{
    BossBar, Error, Holder, MAX_LABEL_BYTES, MAX_LINES, MAX_SIDEBAR_LINES, Result, Scoreboard,
    Slot, Sort,
};
use crate::player;
use crate::protocol::consts::{BossEventOverlay, BossEventType, ScoreboardIdentity};
use crate::protocol::packet::{BossEvent, Packet, RemoveObjective, SetDisplayObjective, SetScore};
use crate::protocol::{MAX_BOSS_EVENT_BOSS_BAR_TITLE, ScoreboardEntry};
use crate::tick::checkpoint;

/// Color codes that fill empty text lines, so that each is its own blank line.
const BLANKS: [&str; 15] = [
    "§1", "§2", "§3", "§4", "§5", "§6", "§7", "§8", "§9", "§a", "§b", "§c", "§d", "§e", "§f",
];

/// What one player was last sent.
#[derive(Debug, Default)]
pub(super) struct Shown {
    /// The scoreboard of each slot, in the order `List`, `Sidebar`, `BelowName` ([`slot_index`]).
    boards: [Option<Board>; 3],
    bar: Option<BossBar>,
}

impl Shown {
    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self { boards, bar } = self;
        for board in boards {
            h.u64(u64::from(board.is_some()));
            if let Some(board) = board {
                let Board {
                    title,
                    sort,
                    entries,
                } = board;
                h.bytes(title.as_bytes());
                h.u64(u64::from(i32::from(*sort).cast_unsigned()));
                h.usize(entries.len());
                for entry in entries {
                    let Entry { id, who, score } = entry;
                    h.u64(id.cast_unsigned());
                    h.u64(u64::from(score.cast_unsigned()));
                    match who {
                        Who::Text(t) => h.bytes(t.as_bytes()),
                        Who::Player(u) => h.u64(u.cast_unsigned()),
                    }
                }
            }
        }
        h.u64(u64::from(bar.is_some()));
        if let Some(bar) = bar {
            let BossBar {
                title,
                progress,
                color,
            } = bar;
            h.bytes(title.as_bytes());
            h.u64(u64::from(progress.to_bits()));
            h.u64(u64::from(u8::from(*color)));
        }
    }
}

checkpoint::fields!(Shown { boards, bar });

/// A scoreboard as sent: its lines resolved to entries.
#[derive(Debug, Clone, PartialEq)]
struct Board {
    title: String,
    sort: Sort,
    entries: Vec<Entry>,
}

checkpoint::fields!(Board {
    title,
    sort,
    entries,
});

/// One line as sent.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: i64,
    who: Who,
    score: i32,
}

checkpoint::fields!(Entry { id, who, score });

/// The identity of an entry.
#[derive(Debug, Clone, PartialEq)]
enum Who {
    /// A fake player showing this text.
    Text(String),
    /// A player, by unique ID.
    Player(i64),
}

checkpoint::variants!(Who {
    0 => Text(text),
    1 => Player(unique_id),
});

/// Whether a `SetScore` entry adds or removes.
#[derive(Debug, Clone, Copy)]
enum Change {
    Add,
    Remove,
}

impl Context<'_> {
    /// Hides the boss bar `player` sees. False if the player is not in the game or sees none.
    pub fn remove_boss_bar(&mut self, player: player::Id) -> bool {
        let Some((p, open)) = open(self.state, player) else {
            return false;
        };
        let Some(old) = open.display.bar.take() else {
            return false;
        };
        let entity = crate::tick::entity::unique_id(p.runtime_id);
        p.send_packet(boss_event(entity, BossEventType::Hide, &old));
        true
    }

    /// Removes the scoreboard `player` sees in `slot`. False if the player is not in the game or
    /// sees none there.
    pub fn remove_scoreboard(&mut self, player: player::Id, slot: Slot) -> bool {
        let Some((p, open)) = open(self.state, player) else {
            return false;
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant: slot_index is below the three slots"
        )]
        if open.display.boards[slot_index(slot)].take().is_none() {
            return false;
        }
        p.send_packet(remove_objective(objective(slot)));
        true
    }

    /// Shows `bar` to `player` in place of the boss bar it saw. Only what changed is sent: a new
    /// title or progress updates the bar, a new color shows it again.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a bar whose title is longer than 256 bytes; nothing is changed.
    /// [`Error::Offline`] for a player who is not in the game, checked after the title; nothing
    /// is changed.
    pub fn set_boss_bar(&mut self, player: player::Id, bar: BossBar) -> Result<()> {
        if bar.title.len() > MAX_BOSS_EVENT_BOSS_BAR_TITLE {
            return Err(Error::TooLong);
        }
        let (p, open) = open(self.state, player).ok_or(Error::Offline)?;
        let entity = crate::tick::entity::unique_id(p.runtime_id);
        match &open.display.bar {
            Some(old) if old.color == bar.color => {
                if old.title != bar.title {
                    p.send_packet(boss_event(entity, BossEventType::Title, &bar));
                }
                if old.progress.to_bits() != bar.progress.to_bits() {
                    p.send_packet(boss_event(entity, BossEventType::HealthPercentage, &bar));
                }
            }
            old => {
                if let Some(old) = old {
                    p.send_packet(boss_event(entity, BossEventType::Hide, old));
                }
                p.send_packet(boss_event(entity, BossEventType::Show, &bar));
            }
        }
        open.display.bar = Some(bar);
        Ok(())
    }

    /// Shows `board` to `player` in its slot, in place of what the slot showed. Only the lines
    /// that changed are sent again, a line being the same when its entry, text or player, and
    /// score are the same wherever it stands in the list; a new title or sort order sends the
    /// whole scoreboard.
    ///
    /// A player line stays on the scoreboards of the other players when its player leaves, so
    /// setting the scoreboard again fails on that line.
    ///
    /// # Errors
    ///
    /// [`Error::TooManyLines`] for more lines than the slot shows (15 in the sidebar, 1,024
    /// elsewhere); nothing is changed.
    /// [`Error::TooLong`] for a title or a text longer than 1,024 bytes; nothing is changed.
    /// [`Error::UnknownPlayer`] for a player line that names a player not in the game; nothing
    /// is changed.
    /// [`Error::DuplicatePlayer`] for a player line that names a player named before; nothing
    /// is changed.
    /// [`Error::Offline`] for a `player` who is not in the game, checked after the scoreboard;
    /// nothing is changed.
    pub fn set_scoreboard(&mut self, player: player::Id, board: Scoreboard) -> Result<()> {
        let max = if board.slot == Slot::Sidebar {
            MAX_SIDEBAR_LINES
        } else {
            MAX_LINES
        };
        if board.lines.len() > max {
            return Err(Error::TooManyLines);
        }
        if board.title.len() > MAX_LABEL_BYTES {
            return Err(Error::TooLong);
        }
        let index = slot_index(board.slot);
        let mut players = BTreeSet::new();
        let mut entries = Vec::with_capacity(board.lines.len());
        let lines = board.lines.into_iter().zip(BLANKS.iter().cycle());
        for (position, (line, blank)) in lines.enumerate() {
            let (id, who) = match line.holder {
                Holder::Text(text) if text.len() > MAX_LABEL_BYTES => return Err(Error::TooLong),
                Holder::Text(text) => {
                    let text = if text.is_empty() {
                        (*blank).to_owned()
                    } else {
                        text
                    };
                    (text_entry_id(index, position), Who::Text(text))
                }
                Holder::Player(p) => {
                    let target = self.player(p).ok_or(Error::UnknownPlayer)?;
                    let unique = crate::tick::entity::unique_id(target.runtime_id);
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
                if old.entries == new.entries {
                    return Ok(());
                }
                changed_entries(old, &new)
            }
            old => {
                if old.is_some() {
                    p.send_packet(remove_objective(name));
                }
                p.send_packet(Packet::SetDisplayObjective(SetDisplayObjective {
                    display_slot: board.slot.as_str().to_owned(),
                    objective_name: name.to_owned(),
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
                    .map(|e| score_entry(name, e, change))
                    .collect();
                p.send_packet(Packet::SetScore(SetScore { entries }));
            }
        }
        *shown = Some(new);
        Ok(())
    }
}

/// The position of `slot` in [`Shown::boards`].
fn slot_index(slot: Slot) -> usize {
    match slot {
        Slot::List => 0,
        Slot::Sidebar => 1,
        Slot::BelowName => 2,
    }
}

/// The entry ID of the text line at `position` of the scoreboard in the slot at `index`:
/// negative, and apart from the lines of every other slot.
#[expect(
    clippy::expect_used,
    reason = "invariant: a text line's offset is below three times the line bound"
)]
fn text_entry_id(index: usize, position: usize) -> i64 {
    -1 - i64::try_from(index * MAX_LINES + position).expect("a text line's offset fits an i64")
}

/// The objective of `slot`.
fn objective(slot: Slot) -> &'static str {
    match slot {
        Slot::List => "anchor:list",
        Slot::Sidebar => "anchor:sidebar",
        Slot::BelowName => "anchor:belowname",
    }
}

/// The entries of `old` to remove and those of `new` to add so that the client shows `new`,
/// compared by entry ID: a line that only changed place in the list is not sent again.
fn changed_entries<'a>(old: &'a Board, new: &'a Board) -> (Vec<&'a Entry>, Vec<&'a Entry>) {
    let old_by_id: BTreeMap<i64, &Entry> = old.entries.iter().map(|e| (e.id, e)).collect();
    let new_by_id: BTreeMap<i64, &Entry> = new.entries.iter().map(|e| (e.id, e)).collect();
    let mut removed = Vec::new();
    for entry in &old.entries {
        if new_by_id.get(&entry.id).copied() != Some(entry) {
            removed.push(entry);
        }
    }
    let mut added = Vec::new();
    for entry in &new.entries {
        if old_by_id.get(&entry.id).copied() != Some(entry) {
            added.push(entry);
        }
    }
    (removed, added)
}

/// The `SetScore` entry of `objective` that adds or removes `e`.
fn score_entry(objective: &str, e: &Entry, change: Change) -> ScoreboardEntry {
    let (identity, entity, name) = match (change, &e.who) {
        (Change::Add, Who::Text(t)) => (ScoreboardIdentity::FakePlayer, 0, t.clone()),
        (Change::Add, &Who::Player(u)) => (ScoreboardIdentity::Player, u, String::new()),
        (Change::Remove, _) => (ScoreboardIdentity::Remove, 0, String::new()),
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

/// The `RemoveObjective` of the objective `name`.
fn remove_objective(name: &str) -> Packet {
    Packet::RemoveObjective(RemoveObjective {
        objective_name: name.to_owned(),
    })
}

/// The `BossEvent` of `kind` that shows `bar` on the entity `entity`.
fn boss_event(entity: i64, kind: BossEventType, bar: &BossBar) -> Packet {
    Packet::BossEvent(BossEvent {
        boss_entity_unique_id: entity,
        event_type: kind.into(),
        boss_bar_title: bar.title.clone(),
        filtered_boss_bar_title: String::new(),
        health_percentage: bar.progress,
        colour: bar.color.into(),
        overlay: BossEventOverlay::Progress.into(),
    })
}
