//! Players: the joined players a [`Context`](crate::Context) reaches, what can be read and
//! changed on them, and the messages they can be shown.
//!
//! Code outside a tick keeps only an [`Id`]; [`Context::player`](crate::Context::player) and
//! [`Context::player_mut`](crate::Context::player_mut) lend the [`Player`] itself. A player who
//! is not online is reached through its saved data: a [`Ticket`] from
//! [`Context::hold_offline_player`](crate::Context::hold_offline_player) and the [`Offline`]
//! record it holds.

use std::fmt;
use std::time::Duration;

use crate::protocol::consts::{TextType, TitleAction};
use crate::protocol::packet::{Packet, SetTitle, Text, ToastRequest};
use crate::protocol::{TEXT_MESSAGE_MAX_LEN, TEXT_PARAMETERS_MAX_LEN, TEXT_SOURCE_NAME_MAX_LEN};
use crate::tick::queue::SessionId;
pub use crate::tick::{Offline, Player};

/// A joined player. It stays valid until the player leaves; a later join of the same name is
/// another player with another `Id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(SessionId);

impl Id {
    /// The player of `session`.
    pub(crate) const fn new(session: SessionId) -> Self {
        Self(session)
    }

    /// The session of the player.
    pub(crate) const fn session(self) -> SessionId {
        self.0
    }
}

/// A hold on the saved data of a player who is not online: from
/// [`Context::hold_offline_player`](crate::Context::hold_offline_player), it keeps the data in
/// memory until [`Context::release_offline_player`](crate::Context::release_offline_player) lets
/// it go or the player joins. A ticket is never given out twice while the server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(u64);

impl Ticket {
    /// The ticket of number `n`.
    pub(crate) const fn new(n: u64) -> Self {
        Self(n)
    }

    /// Its number.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Why a player call changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A text is longer than a client accepts: 65,536 bytes, 256 for a chat source name; or a
    /// world name is longer than a world's: 250 bytes.
    TooLong,
    /// A translated message has more than 4 parameters, the most a client accepts.
    TooManyParameters,
    /// A number is not finite or outside the range the setter documents.
    OutOfRange,
    /// A position is not finite or lies outside the world border.
    OutsideWorld,
    /// The server has no world with that ID.
    UnknownWorld,
    /// The player is already leaving: it was kicked or transferred this tick.
    Leaving,
    /// A transfer address is empty or longer than 253 bytes (the longest host name).
    InvalidAddress,
    /// A server setting turns the system off (`hunger`, `experience` or `drowning` false, or
    /// `save-player-data` false for an offline player).
    Disabled,
    /// A sound has no full-volume form: the client plays it from a level event.
    NoFullVolume,
    /// The player is online or joining, so its data is the online player's
    /// ([`Context::player`](crate::Context::player)), not a saved record.
    Online,
    /// No data is saved for the player: it never joined, or no player can have that name.
    NotSaved,
    /// The player's saved data is damaged; it stays as saved until an operator repairs it.
    Damaged,
    /// 64 offline players' tickets are held or asked for already.
    Full,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "the text is too long",
            Self::TooManyParameters => "a translation takes at most 4 parameters",
            Self::OutOfRange => "the value is out of range",
            Self::OutsideWorld => "the position is outside the world",
            Self::UnknownWorld => "the server has no such world",
            Self::Leaving => "the player is leaving",
            Self::InvalidAddress => "the transfer address is invalid",
            Self::Disabled => "a server setting turns this off",
            Self::NoFullVolume => "the sound has no full-volume form",
            Self::Online => "the player is online",
            Self::NotSaved => "no data is saved for the player",
            Self::Damaged => "the player's saved data is damaged",
            Self::Full => "too many offline players are held",
        })
    }
}

impl std::error::Error for Error {}

/// A result with the player [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The highest experience level (PMMP `Attribute::EXPERIENCE_LEVEL` maximum).
pub(crate) const MAX_LEVEL: u32 = 24_791;

/// The experience points that reach `level` from none (PMMP
/// `ExperienceUtils::getXpToReachLevel`), saturating at `u64::MAX`.
#[doc(alias = "ExperienceUtils")]
#[doc(alias = "ExperienceUtils::getXpToReachLevel")]
pub fn xp_to_reach_level(level: u32) -> u64 {
    let l = u128::from(level);
    let points = if level <= 16 {
        l * l + 6 * l
    } else if level <= 31 {
        // 2.5 l² - 40.5 l + 360, exact in halves: (5 l² - 81 l + 720) / 2, rounded down.
        (5 * l * l + 720 - 81 * l) / 2
    } else {
        // 4.5 l² - 162.5 l + 2220 = (9 l² - 325 l + 4440) / 2.
        (9 * l * l + 4440 - 325 * l) / 2
    };
    u64::try_from(points).unwrap_or(u64::MAX)
}

/// The experience points from `level` to the next (PMMP
/// `ExperienceUtils::getXpToCompleteLevel`).
#[doc(alias = "ExperienceUtils::getXpToCompleteLevel")]
pub fn xp_to_complete_level(level: u32) -> u64 {
    let l = u64::from(level);
    if level <= 15 {
        2 * l + 7
    } else if level <= 30 {
        5 * l - 38
    } else {
        9 * l - 158
    }
}

/// The level `xp` points make, at most [`Player::xp_level`]'s highest (24,791), and the share of
/// the next level's points collected, from 0 to 1 (PMMP `ExperienceUtils::getLevelFromXp`, whose
/// fraction is the root of the level's quadratic instead: Anchor keeps the share, as the client
/// shows it).
#[doc(alias = "ExperienceUtils::getLevelFromXp")]
pub fn level_from_xp(xp: u64) -> (u32, f64) {
    let (mut lo, mut hi) = (0, MAX_LEVEL);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if xp_to_reach_level(mid) <= xp {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let into = xp.saturating_sub(xp_to_reach_level(lo));
    let need = xp_to_complete_level(lo);
    #[expect(
        clippy::cast_precision_loss,
        reason = "invariant: the points into one level stay below its size, at most 222,961, \
                  exact in f64"
    )]
    let progress = if lo >= MAX_LEVEL || need == 0 {
        0.0
    } else {
        into as f64 / need as f64
    };
    (lo, progress)
}

/// The longest text a client decodes in a message, in bytes (gophertunnel `Text` bound): the
/// most a message, a command's output and the other texts the API sends take.
pub const MAX_TEXT: usize = TEXT_MESSAGE_MAX_LEN;
/// The longest chat source name a client decodes.
const MAX_SOURCE: usize = TEXT_SOURCE_NAME_MAX_LEN;
/// The most parameters of a translated message a client decodes.
const MAX_PARAMETERS: usize = TEXT_PARAMETERS_MAX_LEN;

/// Something to show a player with [`Player::send`] or everyone with
/// [`Context::broadcast`](crate::Context::broadcast): PMMP's message methods merged into one.
/// Texts may hold `§` format codes; a key is a client translation key such as
/// `chat.type.text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A chat line shown as it is (PMMP `sendMessage` with a string).
    Raw(String),
    /// A chat line from `source`, shown as `<source> text`.
    Chat {
        /// Who speaks: at most 256 bytes.
        source: String,
        /// What is said.
        text: String,
    },
    /// A chat line the client translates: `key` with up to 4 `parameters` (PMMP `sendMessage`
    /// with a `Translatable`).
    Translation {
        /// The translation key.
        key: String,
        /// The values of the key's placeholders, in order.
        parameters: Vec<String>,
    },
    /// A line above the hotbar (PMMP `sendPopup`).
    Popup(String),
    /// A line above the hotbar, higher than a popup (PMMP `sendTip`).
    Tip(String),
    /// The "now playing" line of a jukebox, translated by the client (PMMP `sendJukeboxPopup`).
    JukeboxPopup {
        /// The translation key.
        key: String,
        /// The values of the key's placeholders, in order.
        parameters: Vec<String>,
    },
    /// A chat line marked as a system message.
    System(String),
    /// A whisper from `source`.
    Whisper {
        /// Who whispers: at most 256 bytes.
        source: String,
        /// What is whispered.
        text: String,
    },
    /// An announcement from `source`, shown as `[source] text`.
    Announcement {
        /// Who announces: at most 256 bytes.
        source: String,
        /// What is announced.
        text: String,
    },
    /// A title in the middle of the screen, with a subtitle under it unless it is empty, and its
    /// timing unless `times` is `None` (PMMP `sendTitle`).
    Title {
        /// The title.
        title: String,
        /// The subtitle; empty for none.
        subtitle: String,
        /// How long the title fades in, stays and fades out; `None` keeps the client's.
        times: Option<TitleTimes>,
    },
    /// The subtitle of the next or current title (PMMP `sendSubTitle`).
    Subtitle(String),
    /// A line above the hotbar in the title style (PMMP `sendActionBarMessage`).
    ActionBar(String),
    /// The timing of the next titles (PMMP `setTitleDuration`).
    TitleTimes(TitleTimes),
    /// Removes the title on screen (PMMP `removeTitles`).
    ClearTitle,
    /// Removes the title on screen and resets the timing to the client's (PMMP `resetTitles`).
    ResetTitle,
    /// A notification at the top of the screen (PMMP `sendToastNotification`).
    Toast {
        /// The first line.
        title: String,
        /// The second line.
        body: String,
    },
}

/// How long a title fades in, stays and fades out. The client counts whole ticks (50 ms): each
/// time is rounded down to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TitleTimes {
    /// How long the title takes to appear.
    pub fade_in: Duration,
    /// How long the title stays.
    pub stay: Duration,
    /// How long the title takes to disappear.
    pub fade_out: Duration,
}

/// The packets of one message: at most three (a title with its timing and subtitle).
pub(crate) type Packets = [Option<Packet>; 3];

impl Message {
    /// The packets that show the message, after checking every text against what a client
    /// decodes.
    pub(crate) fn into_packets(self) -> Result<Packets> {
        let text = |kind: TextType, message: String| {
            check(&message)?;
            Ok(text_packet(kind, String::new(), message, Vec::new()))
        };
        let spoken = |kind: TextType, source: String, message: String| {
            check(&message)?;
            if source.len() > MAX_SOURCE {
                return Err(Error::TooLong);
            }
            Ok(text_packet(kind, source, message, Vec::new()))
        };
        let translated = |kind: TextType, key: String, parameters: Vec<String>| {
            check(&key)?;
            if parameters.len() > MAX_PARAMETERS {
                return Err(Error::TooManyParameters);
            }
            for p in &parameters {
                check(p)?;
            }
            let mut packet = text_packet(kind, String::new(), key, parameters);
            if let Packet::Text(t) = &mut packet {
                t.needs_translation = true;
            }
            Ok(packet)
        };
        let one = |packet: Result<Packet>| packet.map(|p| [Some(p), None, None]);
        match self {
            Self::Raw(m) => one(text(TextType::Raw, m)),
            Self::Chat { source, text } => one(spoken(TextType::Chat, source, text)),
            Self::Translation { key, parameters } => {
                one(translated(TextType::Translation, key, parameters))
            }
            Self::Popup(m) => one(text(TextType::Popup, m)),
            Self::Tip(m) => one(text(TextType::Tip, m)),
            Self::JukeboxPopup { key, parameters } => {
                one(translated(TextType::JukeboxPopup, key, parameters))
            }
            Self::System(m) => one(text(TextType::System, m)),
            Self::Whisper { source, text } => one(spoken(TextType::Whisper, source, text)),
            Self::Announcement { source, text } => {
                one(spoken(TextType::Announcement, source, text))
            }
            Self::Title {
                title,
                subtitle,
                times,
            } => {
                check(&title)?;
                check(&subtitle)?;
                let subtitle = (!subtitle.is_empty())
                    .then(|| title_packet(TitleAction::SetSubtitle, subtitle));
                Ok([
                    times.map(TitleTimes::packet),
                    subtitle,
                    Some(title_packet(TitleAction::SetTitle, title)),
                ])
            }
            Self::Subtitle(m) => {
                check(&m)?;
                one(Ok(title_packet(TitleAction::SetSubtitle, m)))
            }
            Self::ActionBar(m) => {
                check(&m)?;
                one(Ok(title_packet(TitleAction::SetActionBar, m)))
            }
            Self::TitleTimes(times) => one(Ok(times.packet())),
            Self::ClearTitle => one(Ok(title_packet(TitleAction::Clear, String::new()))),
            Self::ResetTitle => one(Ok(title_packet(TitleAction::Reset, String::new()))),
            Self::Toast { title, body } => {
                check(&title)?;
                check(&body)?;
                one(Ok(Packet::ToastRequest(ToastRequest {
                    title,
                    message: body,
                })))
            }
        }
    }
}

impl TitleTimes {
    /// The `SetTitle` packet that sets the timing.
    fn packet(self) -> Packet {
        let ticks = |d: Duration| i32::try_from(d.as_millis() / 50).unwrap_or(i32::MAX);
        Packet::SetTitle(SetTitle {
            action_type: TitleAction::SetDurations.into(),
            fade_in_duration: ticks(self.fade_in),
            remain_duration: ticks(self.stay),
            fade_out_duration: ticks(self.fade_out),
            ..SetTitle::default()
        })
    }
}

/// Fails for a text longer than a client decodes.
fn check(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT {
        return Err(Error::TooLong);
    }
    Ok(())
}

/// A `Text` packet. An empty message becomes a space: clients fail on an empty one (PMMP
/// `TextPacket::messageOnly`).
fn text_packet(kind: TextType, source: String, message: String, parameters: Vec<String>) -> Packet {
    let message = if message.is_empty() {
        " ".to_owned()
    } else {
        message
    };
    Packet::Text(Box::new(Text {
        text_type: kind.into(),
        source_name: source,
        message,
        parameters,
        ..Text::default()
    }))
}

/// A `SetTitle` packet with a text.
fn title_packet(action: TitleAction, text: String) -> Packet {
    Packet::SetTitle(SetTitle {
        action_type: action.into(),
        text,
        ..SetTitle::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(message: Message) -> Vec<Packet> {
        message
            .into_packets()
            .unwrap()
            .into_iter()
            .flatten()
            .collect()
    }

    // pmmp: ExperienceUtils, ExperienceUtils::getXpToReachLevel, ExperienceUtils::getXpToCompleteLevel, ExperienceUtils::getLevelFromXp
    #[test]
    fn level_thresholds_follow_pmmp() {
        // PMMP ExperienceUtils at the three formula boundaries.
        let reach = [
            (0, 0),
            (1, 7),
            (16, 352),
            (17, 394),
            (30, 1395),
            (31, 1507),
            (32, 1628),
        ];
        for (level, points) in reach {
            assert_eq!(xp_to_reach_level(level), points, "level {level}");
        }
        let complete = [(0, 7), (15, 37), (16, 42), (30, 112), (31, 121)];
        for (level, points) in complete {
            assert_eq!(xp_to_complete_level(level), points, "level {level}");
        }
        for level in 0..200 {
            assert_eq!(
                xp_to_reach_level(level + 1) - xp_to_reach_level(level),
                xp_to_complete_level(level),
                "level {level}"
            );
            assert_eq!(level_from_xp(xp_to_reach_level(level)), (level, 0.0));
        }
        let (level, progress) = level_from_xp(xp_to_reach_level(10) + xp_to_complete_level(10) / 2);
        assert_eq!(level, 10);
        assert!((progress - 13.0 / 27.0).abs() < 1e-12);
        // Any input: the top level, and no overflow.
        assert!(xp_to_reach_level(u32::MAX) > xp_to_reach_level(MAX_LEVEL));
        assert_eq!(level_from_xp(u64::MAX), (MAX_LEVEL, 0.0));
    }

    #[test]
    fn texts_map_to_their_text_types_and_empty_ones_become_a_space() {
        let cases = [
            (Message::Raw(String::new()), 0, false),
            (Message::Popup("p".to_owned()), 3, false),
            (Message::Tip("t".to_owned()), 5, false),
            (Message::System("s".to_owned()), 6, false),
            (
                Message::Translation {
                    key: "chat.type.text".to_owned(),
                    parameters: vec!["a".to_owned(), "b".to_owned()],
                },
                2,
                true,
            ),
            (
                Message::JukeboxPopup {
                    key: "record.nowPlaying".to_owned(),
                    parameters: vec!["x".to_owned()],
                },
                4,
                true,
            ),
        ];
        for (message, kind, translated) in cases {
            let [Packet::Text(t)]: [Packet; 1] = packets(message).try_into().unwrap() else {
                panic!("one text packet");
            };
            assert_eq!(t.text_type, kind);
            assert_eq!(t.needs_translation, translated);
            assert!(!t.message.is_empty());
            // Every text encodes: the client decodes what the codec writes.
            Packet::Text(t).encode(&mut Vec::new(), 0).unwrap();
        }
    }

    #[test]
    fn titles_send_their_timing_and_subtitle_first() {
        let times = TitleTimes {
            fade_in: Duration::from_millis(500),
            stay: Duration::MAX,
            fade_out: Duration::from_millis(299),
        };
        let sent = packets(Message::Title {
            title: "T".to_owned(),
            subtitle: "S".to_owned(),
            times: Some(times),
        });
        let actions: Vec<(i32, i32)> = sent
            .iter()
            .map(|p| match p {
                Packet::SetTitle(t) => (t.action_type, t.remain_duration),
                _ => panic!("titles only"),
            })
            .collect();
        assert_eq!(actions, [(5, i32::MAX), (3, 0), (2, 0)]);
        let plain = packets(Message::Title {
            title: "T".to_owned(),
            subtitle: String::new(),
            times: None,
        });
        assert_eq!(plain.len(), 1);
        for (message, action) in [
            (Message::Subtitle("s".to_owned()), 3),
            (Message::ActionBar("a".to_owned()), 4),
            (Message::ClearTitle, 0),
            (Message::ResetTitle, 1),
        ] {
            assert!(
                matches!(&packets(message)[..], [Packet::SetTitle(t)] if t.action_type == action)
            );
        }
        let toast = packets(Message::Toast {
            title: "a".to_owned(),
            body: "b".to_owned(),
        });
        assert!(matches!(&toast[..], [Packet::ToastRequest(t)] if t.message == "b"));
    }

    #[test]
    fn texts_beyond_what_a_client_decodes_are_refused() {
        let long = "x".repeat(MAX_TEXT + 1);
        assert_eq!(
            Message::Raw(long.clone()).into_packets(),
            Err(Error::TooLong)
        );
        assert_eq!(
            Message::Chat {
                source: "s".repeat(MAX_SOURCE + 1),
                text: "hi".to_owned()
            }
            .into_packets(),
            Err(Error::TooLong)
        );
        assert_eq!(
            Message::Translation {
                key: "k".to_owned(),
                parameters: vec![String::new(); 5]
            }
            .into_packets(),
            Err(Error::TooManyParameters)
        );
        assert_eq!(
            Message::Title {
                title: String::new(),
                subtitle: long,
                times: None
            }
            .into_packets(),
            Err(Error::TooLong)
        );
        assert!(Message::Raw("x".repeat(MAX_TEXT)).into_packets().is_ok());
    }
}
