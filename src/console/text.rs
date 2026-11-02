//! What the console shows for a message a client translates: Anchor's own sentence for each
//! translation key the server sends (no Mojang or other server sentences), filled
//! with the key's parameters. Players get the key and their client translates it; the console
//! log reads the sentence. Every path to the console uses [`render`]: command results and
//! errors, the copies of command results, and messages sent to the console.

use crate::player::Message;

/// Anchor's sentences by translation key, in key order; `{%0}`, `{%1}`... are the parameters.
const TEXTS: [(&str, &str); 97] = [
    ("chat.type.admin", "[{%0}: {%1}]"),
    ("chat.type.announcement", "[{%0}] {%1}"),
    ("chat.type.text", "<{%0}> {%1}"),
    (
        "commands.allowlist.add.success",
        "{%0} is on the whitelist now",
    ),
    (
        "commands.allowlist.description",
        "Changes or shows the whitelist",
    ),
    ("commands.allowlist.disabled", "The whitelist is off"),
    ("commands.allowlist.enabled", "The whitelist is on"),
    ("commands.allowlist.list", "Names on the whitelist: {%0}"),
    (
        "commands.allowlist.remove.success",
        "{%0} is off the whitelist now",
    ),
    ("commands.ban.description", "Bans a name from the server"),
    ("commands.ban.success", "{%0} is banned"),
    (
        "commands.banip.description",
        "Bans an IP address from the server",
    ),
    (
        "commands.banip.invalid",
        "Give an IP address or the name of an online player",
    ),
    ("commands.banip.success", "The address {%0} is banned"),
    (
        "commands.banip.success.players",
        "The address {%0} is banned, used by {%1}",
    ),
    ("commands.banlist.ips", "Banned addresses: {%0}"),
    ("commands.banlist.players", "Banned names: {%0}"),
    ("commands.clear.description", "Takes items from inventories"),
    (
        "commands.clear.failure.no.items",
        "{%0} has no items to take",
    ),
    ("commands.clear.success", "Took {%1} items from {%0}"),
    ("commands.clear.testing", "{%0} holds {%1} such items"),
    ("commands.deop.description", "Takes operator status away"),
    ("commands.deop.message", "You are an operator no more"),
    ("commands.deop.success", "{%0} is an operator no more"),
    ("commands.gamemode.description", "Sets a player's game mode"),
    ("commands.gamemode.success.other", "{%1} plays in {%0} now"),
    ("commands.gamemode.success.self", "You play in {%0} now"),
    (
        "commands.generic.error.permissions",
        "/{%0} is not allowed for you",
    ),
    (
        "commands.generic.invalidOrigin",
        "Only a player may do this",
    ),
    (
        "commands.generic.noTargetMatch",
        "The selector matched no player",
    ),
    ("commands.generic.num.tooBig", "{%0} is more than {%1}"),
    ("commands.generic.num.tooSmall", "{%0} is less than {%1}"),
    (
        "commands.generic.player.notFound",
        "No online player has that name",
    ),
    (
        "commands.generic.syntax",
        "The line does not fit here: {%0}>>{%1}<<{%2}",
    ),
    ("commands.generic.unknown", "No command is named {%0}"),
    ("commands.generic.usage", "Usage: {%0}"),
    ("commands.give.description", "Gives a player items"),
    ("commands.give.success", "{%2} got {%1} of {%0}"),
    ("commands.give.successRecipient", "You got {%1} of {%0}"),
    (
        "commands.kick.description",
        "Removes a player from the server",
    ),
    ("commands.kick.success", "{%0} was kicked"),
    ("commands.kick.success.reason", "{%0} was kicked: {%1}"),
    ("commands.kill.description", "Kills players"),
    ("commands.kill.successful", "{%0} was killed"),
    ("commands.list.description", "Lists the online players"),
    ("commands.message.display.incoming", "{%0} whispers: {%1}"),
    (
        "commands.message.display.outgoing",
        "Whispered to {%0}: {%1}",
    ),
    ("commands.op.description", "Makes a player an operator"),
    ("commands.op.message", "You are an operator now"),
    ("commands.op.success", "{%0} is an operator now"),
    ("commands.players.list", "Players online: {%0} of {%1}"),
    ("commands.save-on.description", "Turns automatic saving on"),
    ("commands.save.disabled", "Automatic saving is off"),
    ("commands.save.enabled", "Automatic saving is on"),
    ("commands.save.start", "Saving"),
    ("commands.stop.description", "Stops the server"),
    ("commands.stop.start", "Stopping the server"),
    ("commands.tp.description", "Moves players"),
    ("commands.tp.success", "Moved {%0} to {%1}"),
    (
        "commands.tp.success.coordinates",
        "Moved {%0} to {%1}, {%2}, {%3}",
    ),
    ("commands.tp.successVictim", "You were moved to {%0}"),
    ("commands.unban.success", "The ban on {%0} is lifted"),
    ("commands.unbanip.invalid", "Give an IP address"),
    (
        "commands.unbanip.success",
        "The ban on the address {%0} is lifted",
    ),
    ("createWorldScreen.gameMode.adventure", "Adventure"),
    ("createWorldScreen.gameMode.creative", "Creative"),
    ("createWorldScreen.gameMode.spectator", "Spectator"),
    ("createWorldScreen.gameMode.survival", "Survival"),
    ("effect.darkness", "Darkness"),
    ("gameMode.changed", "Your game mode is {%0} now"),
    ("multiplayer.player.joined", "{%0} came in"),
    ("multiplayer.player.left", "{%0} went out"),
    ("potion.absorption", "Absorption"),
    ("potion.blindness", "Blindness"),
    ("potion.conduitPower", "Conduit Power"),
    ("potion.confusion", "Nausea"),
    ("potion.damageBoost", "Strength"),
    ("potion.digSlowDown", "Mining Fatigue"),
    ("potion.digSpeed", "Haste"),
    ("potion.fireResistance", "Fire Resistance"),
    ("potion.harm", "Instant Damage"),
    ("potion.heal", "Instant Health"),
    ("potion.healthBoost", "Health Boost"),
    ("potion.hunger", "Hunger"),
    ("potion.invisibility", "Invisibility"),
    ("potion.jump", "Jump Boost"),
    ("potion.levitation", "Levitation"),
    ("potion.moveSlowdown", "Slowness"),
    ("potion.moveSpeed", "Speed"),
    ("potion.nightVision", "Night Vision"),
    ("potion.poison", "Poison"),
    ("potion.regeneration", "Regeneration"),
    ("potion.resistance", "Resistance"),
    ("potion.saturation", "Saturation"),
    ("potion.waterBreathing", "Water Breathing"),
    ("potion.weakness", "Weakness"),
    ("potion.wither", "Wither"),
];

/// What the console shows for `message` sent to it.
pub(crate) fn message(message: &Message) -> String {
    match message {
        Message::Raw(text)
        | Message::Popup(text)
        | Message::Tip(text)
        | Message::System(text)
        | Message::Subtitle(text)
        | Message::ActionBar(text) => text.clone(),
        Message::Translation { key, parameters } | Message::JukeboxPopup { key, parameters } => {
            render(key, parameters)
        }
        Message::Chat { source, text } => render("chat.type.text", &[source, text]),
        Message::Whisper { source, text } => {
            render("commands.message.display.incoming", &[source, text])
        }
        Message::Announcement { source, text } => render("chat.type.announcement", &[source, text]),
        Message::Title {
            title, subtitle, ..
        } if subtitle.is_empty() => title.clone(),
        Message::Title {
            title, subtitle, ..
        } => format!("{title} / {subtitle}"),
        other => format!("{other:?}"),
    }
}

/// What the console shows for `text` (a translation key or a text) and its `params`, as the
/// client would show it: a key becomes Anchor's sentence; a text keeps its words and each `%key`
/// in it becomes the sentence (format codes stay for the log to leave out). Without a sentence,
/// the text is kept and the parameters follow it in brackets.
pub(crate) fn render<S: AsRef<str>>(text: &str, params: &[S]) -> String {
    let params: Vec<&str> = params.iter().map(|p| param(p.as_ref())).collect();
    if let Some(template) = sentence(text) {
        return fill(template, &params);
    }
    let mut out = String::with_capacity(text.len());
    let mut found = false;
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        let (before, after) = rest.split_at(at);
        out.push_str(before);
        let tail = after.get(1..).unwrap_or_default();
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
            .unwrap_or(tail.len());
        let (key, next) = tail.split_at(end);
        if let Some(template) = sentence(key) {
            out.push_str(&fill(template, &params));
            found = true;
        } else {
            out.push('%');
            out.push_str(key);
        }
        rest = next;
    }
    out.push_str(rest);
    if !found && !params.is_empty() {
        out.push_str(" (");
        out.push_str(&params.join(", "));
        out.push(')');
    }
    out
}

/// A parameter as the console reads it: a `%key` with a sentence becomes the sentence.
fn param(text: &str) -> &str {
    text.strip_prefix('%').and_then(sentence).unwrap_or(text)
}

/// `template` with `{%0}`, `{%1}`... replaced by `params`, in one pass so that a parameter's own
/// text is kept as it is; a placeholder without a parameter stays.
pub(crate) fn fill<S: AsRef<str>>(template: &str, params: &[S]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((before, after)) = rest.split_once("{%") {
        out.push_str(before);
        let param = after
            .split_once('}')
            .and_then(|(digits, tail)| Some((params.get(digits.parse::<usize>().ok()?)?, tail)));
        if let Some((param, tail)) = param {
            out.push_str(param.as_ref());
            rest = tail;
        } else {
            out.push_str("{%");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Anchor's sentence for `key`, if it has one.
fn sentence(key: &str) -> Option<&'static str> {
    TEXTS
        .binary_search_by(|(k, _)| k.cmp(&key))
        .ok()
        .and_then(|i| TEXTS.get(i))
        .map(|&(_, text)| text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sources whose translation keys can reach the console: the commands and the
    /// messages a player's coming and going sends.
    const SOURCES: [&str; 11] = [
        include_str!("../command.rs"),
        include_str!("../tick/command.rs"),
        include_str!("../tick/command/builtin.rs"),
        include_str!("../tick/command/builtin/check.rs"),
        include_str!("../tick/command/builtin/lists.rs"),
        include_str!("../tick/command/builtin/players.rs"),
        include_str!("../tick/command/builtin/server.rs"),
        include_str!("../tick/chat.rs"),
        include_str!("../tick/player.rs"),
        include_str!("../tick/spawn.rs"),
        include_str!("../player.rs"),
    ];

    /// The key families a client translates that these sources send.
    const FAMILIES: [&str; 7] = [
        "commands.",
        "chat.type.",
        "gameMode.",
        "createWorldScreen.gameMode.",
        "multiplayer.player.",
        "potion.",
        "effect.",
    ];

    /// The translation keys `source` sends: a key of [`FAMILIES`] right after a `"` (a string
    /// literal) or a `%` (a key inside a text), to the first character a key does not hold.
    fn keys(source: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for family in FAMILIES {
            for (at, _) in source.match_indices(family) {
                let before = source.get(..at).and_then(|s| s.chars().next_back());
                if !matches!(before, Some('"' | '%')) {
                    continue;
                }
                let rest = source.get(at..).unwrap_or_default();
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                    .unwrap_or(rest.len());
                let key = rest.get(..end).unwrap_or_default().trim_end_matches('.');
                if key.len() > family.len() {
                    out.push(key);
                }
            }
        }
        out
    }

    #[test]
    fn every_key_the_server_sends_has_a_console_sentence() {
        for w in TEXTS.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
        let mut missing: Vec<&str> = SOURCES
            .iter()
            .flat_map(|s| keys(s))
            .filter(|k| sentence(k).is_none())
            .collect();
        missing.extend(
            crate::effect::Type::all()
                .iter()
                .map(|t| t.translation_key())
                .filter(|k| sentence(k).is_none()),
        );
        missing.sort_unstable();
        missing.dedup();
        assert!(missing.is_empty(), "no console sentence: {missing:?}");
        assert!(
            keys(SOURCES[5]).contains(&"commands.give.success"),
            "the scan finds keys"
        );
    }

    #[test]
    fn keys_become_sentences_and_texts_keep_their_words() {
        assert_eq!(
            render("commands.players.list", &["2", "20"]),
            "Players online: 2 of 20"
        );
        assert_eq!(
            render(
                "commands.give.success",
                &["%potion.moveSpeed", "3", "Steve"]
            ),
            "Steve got 3 of Speed",
            "a %key parameter is translated too"
        );
        assert_eq!(
            render("§e%multiplayer.player.joined", &["Steve"]),
            "§eSteve came in"
        );
        assert_eq!(render("plain text", &[] as &[&str]), "plain text");
        assert_eq!(render("100% sure", &[] as &[&str]), "100% sure");
        assert_eq!(render("no.such.key", &["a", "b"]), "no.such.key (a, b)");
        assert_eq!(
            message(&Message::Translation {
                key: "commands.op.success".to_owned(),
                parameters: vec!["Steve".to_owned()],
            }),
            "Steve is an operator now"
        );
    }

    #[test]
    fn placeholders_are_filled_once() {
        assert_eq!(
            fill("a {%0} b {%1} {%2}", &["{%1}", "x"]),
            "a {%1} b x {%2}"
        );
    }
}
