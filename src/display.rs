//! Displays: the scoreboards and the boss bar each player sees.
//!
//! Each player has its own: [`Context::set_scoreboard`](crate::Context::set_scoreboard) shows a
//! [`Scoreboard`] in one [`Slot`] and [`Context::set_boss_bar`](crate::Context::set_boss_bar) a
//! [`BossBar`]. Setting one again sends only what changed:
//!
//! ```
//! use anchor::Context;
//! use anchor::display::{BossBar, Colour, Scoreboard, Slot};
//! use anchor::player;
//!
//! fn show(cx:&mut Context<'_>, player: player::Id, online: i32) {
//!     let board = Scoreboard::new(Slot::Sidebar, "Lobby")
//!         .line("Online:", 0)
//!         .line(format!("{online} players"), 1);
//!     let _ = cx.set_scoreboard(player, board);
//!     let bar = BossBar::new("Next game")
//!         .with_progress(0.5)
//!         .with_colour(Colour::Blue);
//!     let _ = cx.set_boss_bar(player, bar);
//! }
//! ```

pub use crate::protocol::consts::{
    BossEventColour as Colour, ScoreboardSlot as Slot, ScoreboardSortOrder as Sort,
};

use std::fmt;

use crate::player;

/// The most lines of a list or below-name scoreboard: the protocol's list bound.
pub(crate) const MAX_LINES: usize = crate::protocol::MAX_LIST_LEN;
/// The most lines of a sidebar: the client shows no more.
pub(crate) const MAX_SIDEBAR_LINES: usize = 15;
/// The longest title or line text, in bytes.
pub(crate) const MAX_TEXT: usize = 1024;

/// A scoreboard: a title and lines, each a text or a player with a score, shown in one slot.
///
/// The client orders the lines by score ([`Sort`], ascending unless set). A text line shows its
/// text; a player line shows the player's name in the sidebar, its score next to its name in the
/// player list ([`Slot::List`]) or below its name tag ([`Slot::BelowName`]). An empty text line
/// is shown blank (filled with a colour code, as dragonfly does).
#[derive(Debug, Clone, PartialEq)]
pub struct Scoreboard {
    pub(crate) slot: Slot,
    pub(crate) title: String,
    pub(crate) sort: Sort,
    pub(crate) lines: Vec<Line>,
}

impl Scoreboard {
    /// A scoreboard titled `title` in `slot`, without lines, sorted ascending.
    pub fn new(slot: Slot, title: impl Into<String>) -> Self {
        Self {
            slot,
            title: title.into(),
            sort: Sort::Ascending,
            lines: Vec::new(),
        }
    }

    /// The scoreboard with its lines ordered by score as `sort` says.
    #[must_use]
    pub fn with_sort(mut self, sort: Sort) -> Self {
        self.sort = sort;
        self
    }

    /// Adds a line showing `text` with `score`.
    #[must_use]
    pub fn line(mut self, text: impl Into<String>, score: i32) -> Self {
        self.lines.push(Line {
            holder: Holder::Text(text.into()),
            score,
        });
        self
    }

    /// Adds a line for `player` with `score`. A player may have one line per scoreboard.
    #[must_use]
    pub fn player(mut self, player: player::Id, score: i32) -> Self {
        self.lines.push(Line {
            holder: Holder::Player(player),
            score,
        });
        self
    }
}

/// One line of a scoreboard.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Line {
    pub(crate) holder: Holder,
    pub(crate) score: i32,
}

/// Whose score a line shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Holder {
    Text(String),
    Player(player::Id),
}

/// A boss bar: a title over a bar filled to its progress.
///
/// Bedrock ties a boss bar to an entity; Anchor uses the player's own entity, whose bar the
/// client lets the server set freely, as PMMP plugins and dragonfly do.
#[derive(Debug, Clone, PartialEq)]
pub struct BossBar {
    pub(crate) title: String,
    pub(crate) progress: f32,
    pub(crate) colour: Colour,
}

impl BossBar {
    /// A full purple bar titled `title`.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            progress: 1.0,
            colour: Colour::Purple,
        }
    }

    /// The bar in `colour`.
    #[must_use]
    pub fn with_colour(mut self, colour: Colour) -> Self {
        self.colour = colour;
        self
    }

    /// The bar filled to `progress`, from 0 (empty) to 1 (full); values outside are clamped and
    /// NaN is 0.
    #[must_use]
    pub fn with_progress(mut self, progress: f32) -> Self {
        self.progress = if progress > 1.0 {
            1.0
        } else if progress > 0.0 {
            progress
        } else {
            0.0
        };
        self
    }
}

/// Why a scoreboard or boss bar was not shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The player is not in the game: it has left, or has not joined yet.
    Offline,
    /// The scoreboard has more lines than its slot shows (15 in the sidebar, 1,024 elsewhere).
    TooManyLines,
    /// A title or line text is longer than 1,024 bytes, or a boss bar title longer than 256.
    TooLong,
    /// A player line names a player who is not in the game.
    UnknownPlayer,
    /// Two lines of the scoreboard name the same player.
    DuplicatePlayer,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Offline => "the player is not in the game",
            Self::TooManyLines => "the scoreboard has more lines than its slot shows",
            Self::TooLong => "a title or line is longer than a client takes",
            Self::UnknownPlayer => "a scoreboard line names a player who is not in the game",
            Self::DuplicatePlayer => "two scoreboard lines name the same player",
        })
    }
}

impl std::error::Error for Error {}

/// A result with the display [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_clamped_and_finite() {
        let p = |v: f32| BossBar::new("").with_progress(v).progress;
        assert_eq!(p(0.25), 0.25);
        assert_eq!(p(1.5), 1.0);
        assert_eq!(p(-1.0), 0.0);
        assert_eq!(p(f32::NAN), 0.0);
        assert_eq!(p(f32::INFINITY), 1.0);
        assert_eq!(p(-0.0).to_bits(), 0.0_f32.to_bits());
    }
}
