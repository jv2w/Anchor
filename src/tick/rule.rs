//! Anchor's own behaviours a server can turn off to send what it needs itself
//! ([`ServerBuilder::without`](crate::ServerBuilder::without)).

/// A behaviour of Anchor's that [`ServerBuilder::without`](crate::ServerBuilder::without) turns
/// off. Each is on unless turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Players' movement and look to the players who see them (`MoveActorAbsolute` from entity
    /// tracking). Off, the server sends players' movement itself; their spawn, removal and
    /// teleports (`MovePlayer`) still go out.
    PlayerMovementBroadcast,
    /// The arm swing (`ActorEvent` `StartAttacking`) the viewers see when a player attacks or
    /// swings at nothing. Off, the server shows swings itself, as BDS relays the client's
    /// `Animate`.
    ServerSwing,
    /// The health attribute rounded up to whole half hearts, as PMMP sends it. Off, the exact
    /// value (in f32), as BDS sends it.
    RoundedHealth,
}

impl Rule {
    /// The rule's bit in a [`RuleSet`].
    const fn bit(self) -> u8 {
        match self {
            Self::PlayerMovementBroadcast => 1,
            Self::ServerSwing => 2,
            Self::RoundedHealth => 4,
        }
    }
}

/// The rules turned off, fixed at build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RuleSet {
    off: u8,
}

impl RuleSet {
    /// Turns `rule` off.
    pub(crate) fn turn_off(&mut self, rule: Rule) {
        self.off |= rule.bit();
    }

    /// Whether `rule` is on.
    pub(crate) fn is_on(self, rule: Rule) -> bool {
        self.off & rule.bit() == 0
    }
}
