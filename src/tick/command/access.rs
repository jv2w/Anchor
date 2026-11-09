//! The operator list, the name and IP bans and the whitelist in the tick.
//!
//! Loaded from the server file at start and kept whole in memory. Every change is recorded by its
//! key until the next save snapshot takes it, so a commit writes each changed entry once, as its
//! last value: a write is absolute, and the same change twice writes the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;

use crate::permission::{self, BanEntry, MAX_ENTRIES, MAX_NAME_BYTES};
use crate::save::{AccessData, AccessWrite};
use crate::storage::{AccessKey, AccessKind};
use crate::tick::checkpoint::{self, Codec, Reader, Writer};

/// The operator list, the bans and the whitelist, which a checkpoint shares with the state until
/// the next change and encodes only when a crash log is written.
#[derive(Debug, Clone, Default)]
pub(crate) struct Lists {
    /// Operator names, ASCII lowercase.
    ops: BTreeSet<String>,
    /// Name bans by name (ASCII lowercase), expired ones included until a ban list change.
    bans: BTreeMap<String, BanEntry>,
    /// IP bans by address.
    ip_bans: BTreeMap<IpAddr, BanEntry>,
    /// Whitelisted names, ASCII lowercase.
    whitelist: BTreeSet<String>,
}

impl Lists {
    /// The entries in all four lists.
    pub(crate) fn len(&self) -> usize {
        self.ops.len() + self.bans.len() + self.ip_bans.len() + self.whitelist.len()
    }
}

checkpoint::fields!(Lists {
    ops,
    bans,
    ip_bans,
    whitelist,
});

/// The lists and the changes no snapshot holds yet.
#[derive(Debug, Default)]
pub(super) struct Access {
    /// The operator list, the bans and the whitelist.
    lists: Arc<Lists>,
    /// Whether only whitelisted players (and operators) may join (`whitelist`; not saved).
    whitelist_enabled: bool,
    /// The last write of every entry changed since the last snapshot.
    changes: BTreeMap<AccessKey, AccessWrite>,
    /// The end tick and name of every name ban that ends, in end order: a ban list change drops
    /// the ended ones without looking at the others.
    ban_ends: BTreeSet<(u64, String)>,
    /// The end tick and address of every IP ban that ends, in end order.
    ip_ban_ends: BTreeSet<(u64, IpAddr)>,
}

impl Access {
    /// The lists the server file held, with the whitelist on or off as the settings say.
    pub(super) fn new(data: AccessData, whitelist_enabled: bool) -> Self {
        let AccessData {
            ops,
            bans,
            ip_bans,
            whitelist,
        } = data;
        let mut access = Self {
            lists: Arc::new(Lists {
                ops: ops.into_iter().collect(),
                bans: bans.into_iter().map(|e| (e.name().to_owned(), e)).collect(),
                ip_bans: ip_bans
                    .into_iter()
                    .map(|e| {
                        #[expect(
                            clippy::expect_used,
                            reason = "invariant: the server file reads an IP ban with its address"
                        )]
                        let ip = e.ip_address().expect("an IP ban has an address");
                        (ip, e)
                    })
                    .collect(),
                whitelist: whitelist.into_iter().collect(),
            }),
            whitelist_enabled,
            changes: BTreeMap::new(),
            ban_ends: BTreeSet::new(),
            ip_ban_ends: BTreeSet::new(),
        };
        access.index_ends();
        access
    }

    /// The ban of `name` (any ASCII case) in force at `tick`.
    pub(super) fn ban_entry(&self, name: &str, tick: u64) -> Option<&BanEntry> {
        folded(name, |n| self.lists.bans.get(n))
            .flatten()
            .filter(|e| !e.has_expired(tick))
    }

    /// The name bans in force at `tick`, in name order.
    pub(super) fn bans(&self, tick: u64) -> impl Iterator<Item = &BanEntry> {
        self.lists
            .bans
            .values()
            .filter(move |e| !e.has_expired(tick))
    }

    /// Whether `name` may join as far as the whitelist goes: the whitelist is off, or `name` is
    /// on it or an operator.
    pub(super) fn can_join(&self, name: &str) -> bool {
        !self.whitelist_enabled || self.is_op(name) || self.is_listed(name)
    }

    /// Hashes every field but the end indexes, which the bans give (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            lists,
            whitelist_enabled,
            changes,
            // Derived from `bans`.
            ban_ends: _,
            // Derived from `ip_bans`.
            ip_ban_ends: _,
        } = self;
        let Lists {
            ops,
            bans,
            ip_bans,
            whitelist,
        } = &**lists;
        let ban = |h: &mut crate::tick::Fnv, e: &BanEntry| {
            h.bytes(e.name().as_bytes());
            h.bytes(e.reason().unwrap_or_default().as_bytes());
            h.bytes(e.source().as_bytes());
            h.u64(e.created());
            h.u64(e.expires().map_or(0, |t| t.wrapping_add(1)));
        };
        h.usize(ops.len());
        for name in ops {
            h.bytes(name.as_bytes());
        }
        h.usize(bans.len());
        for entry in bans.values() {
            ban(h, entry);
        }
        h.usize(ip_bans.len());
        for entry in ip_bans.values() {
            ban(h, entry);
        }
        h.usize(whitelist.len());
        for name in whitelist {
            h.bytes(name.as_bytes());
        }
        h.u64(u64::from(*whitelist_enabled));
        h.usize(changes.len());
        for (key, write) in changes {
            h.bytes(&key.to_bytes());
            match write {
                AccessWrite::Delete => h.u64(0),
                AccessWrite::Mark => h.u64(1),
                AccessWrite::Ban(entry) => {
                    h.u64(2);
                    ban(h, entry);
                }
            }
        }
    }

    /// The ban of `ip` in force at `tick`; an IPv4 address written as IPv6 is the IPv4 address,
    /// the form [`BanEntry::ip`] keeps.
    pub(super) fn ip_ban_entry(&self, ip: IpAddr, tick: u64) -> Option<&BanEntry> {
        self.lists
            .ip_bans
            .get(&ip.to_canonical())
            .filter(|e| !e.has_expired(tick))
    }

    /// The IP bans in force at `tick`, in address order.
    pub(super) fn ip_bans(&self, tick: u64) -> impl Iterator<Item = &BanEntry> {
        self.lists
            .ip_bans
            .values()
            .filter(move |e| !e.has_expired(tick))
    }

    /// Whether `name` (any ASCII case) is an operator. Asked for every permission check, so it
    /// does not allocate ([`folded`]).
    pub(super) fn is_op(&self, name: &str) -> bool {
        folded(name, |n| self.lists.ops.contains(n)).unwrap_or(false)
    }

    /// Whether the whitelist is on.
    pub(super) fn is_whitelist_enabled(&self) -> bool {
        self.whitelist_enabled
    }

    /// The operator list, the bans and the whitelist, shared with the state until the next
    /// change.
    pub(super) fn lists(&self) -> Arc<Lists> {
        Arc::clone(&self.lists)
    }

    /// The operators, ASCII lowercase, in name order.
    pub(super) fn ops(&self) -> impl Iterator<Item = &str> {
        self.lists.ops.iter().map(String::as_str)
    }

    /// The whitelisted names, ASCII lowercase, in name order.
    pub(super) fn whitelisted(&self) -> impl ExactSizeIterator<Item = &str> {
        self.lists.whitelist.iter().map(String::as_str)
    }

    /// Adds or replaces a ban made at engine tick `tick`, after dropping the name and IP bans that
    /// have ended.
    pub(super) fn ban(&mut self, entry: BanEntry, tick: u64) -> permission::Result<()> {
        let entry = entry.checked(tick)?;
        self.drop_expired(tick);
        let end = entry.expires();
        if let Some(ip) = entry.ip_address() {
            if !self.lists.ip_bans.contains_key(&ip) && self.lists.ip_bans.len() >= MAX_ENTRIES {
                return Err(permission::Error::Full);
            }
            self.record(
                AccessKind::IpBan,
                entry.name().to_owned(),
                AccessWrite::Ban(entry.clone()),
            );
            if let Some(old) = Arc::make_mut(&mut self.lists).ip_bans.insert(ip, entry)
                && let Some(old_end) = old.expires()
            {
                self.ip_ban_ends.remove(&(old_end, ip));
            }
            if let Some(end) = end {
                self.ip_ban_ends.insert((end, ip));
            }
        } else {
            if !self.lists.bans.contains_key(entry.name()) && self.lists.bans.len() >= MAX_ENTRIES {
                return Err(permission::Error::Full);
            }
            let name = entry.name().to_owned();
            self.record(
                AccessKind::Ban,
                name.clone(),
                AccessWrite::Ban(entry.clone()),
            );
            if let Some(old) = Arc::make_mut(&mut self.lists)
                .bans
                .insert(name.clone(), entry)
                && let Some(old_end) = old.expires()
            {
                self.ban_ends.remove(&(old_end, name.clone()));
            }
            if let Some(end) = end {
                self.ban_ends.insert((end, name));
            }
        }
        Ok(())
    }

    /// Takes operator status from `name`; true if it had it.
    pub(super) fn deop(&mut self, name: &str) -> bool {
        self.remove_name(AccessKind::Op, name)
    }

    /// Makes `name` an operator; true if it was not one.
    pub(super) fn op(&mut self, name: &str) -> permission::Result<bool> {
        self.add_name(AccessKind::Op, name)
    }

    /// Puts in the lists of a checkpoint.
    pub(super) fn set_lists(&mut self, lists: Arc<Lists>) {
        self.lists = lists;
        self.index_ends();
    }

    /// Turns the whitelist on or off.
    pub(super) fn set_whitelist_enabled(&mut self, enabled: bool) {
        self.whitelist_enabled = enabled;
    }

    /// Takes the changes since the last snapshot, each entry once with its last value, in key
    /// order.
    pub(super) fn take_changes(&mut self) -> Vec<(AccessKey, AccessWrite)> {
        std::mem::take(&mut self.changes).into_iter().collect()
    }

    /// Lifts the ban of `name`; true if there was one, ended or not.
    pub(super) fn unban(&mut self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let Some(entry) = Arc::make_mut(&mut self.lists).bans.remove(&name) else {
            return false;
        };
        if let Some(end) = entry.expires() {
            self.ban_ends.remove(&(end, name.clone()));
        }
        self.record(AccessKind::Ban, name, AccessWrite::Delete);
        true
    }

    /// Lifts the ban of `ip` (an IPv4 address written as IPv6 is the IPv4 address); true if there
    /// was one, ended or not.
    pub(super) fn unban_ip(&mut self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        let Some(entry) = Arc::make_mut(&mut self.lists).ip_bans.remove(&ip) else {
            return false;
        };
        if let Some(end) = entry.expires() {
            self.ip_ban_ends.remove(&(end, ip));
        }
        self.record(AccessKind::IpBan, ip.to_string(), AccessWrite::Delete);
        true
    }

    /// Whitelists `name`; true if it was not listed.
    pub(super) fn whitelist_add(&mut self, name: &str) -> permission::Result<bool> {
        self.add_name(AccessKind::Whitelist, name)
    }

    /// Removes `name` from the whitelist; true if it was listed.
    pub(super) fn whitelist_remove(&mut self, name: &str) -> bool {
        self.remove_name(AccessKind::Whitelist, name)
    }

    /// Whether `name` (any ASCII case) is on the whitelist.
    fn is_listed(&self, name: &str) -> bool {
        folded(name, |n| self.lists.whitelist.contains(n)).unwrap_or(false)
    }

    /// Adds `name` to `kind`'s list (operators or whitelist); true if it was not there.
    fn add_name(&mut self, kind: AccessKind, name: &str) -> permission::Result<bool> {
        permission::check_name(name)?;
        let name = name.to_ascii_lowercase();
        let list = if kind == AccessKind::Op {
            &self.lists.ops
        } else {
            &self.lists.whitelist
        };
        if list.contains(&name) {
            return Ok(false);
        }
        if list.len() >= MAX_ENTRIES {
            return Err(permission::Error::Full);
        }
        let lists = Arc::make_mut(&mut self.lists);
        if kind == AccessKind::Op {
            &mut lists.ops
        } else {
            &mut lists.whitelist
        }
        .insert(name.clone());
        self.record(kind, name, AccessWrite::Mark);
        Ok(true)
    }

    /// Drops the bans that have ended at `tick`, found from the front of the end indexes.
    fn drop_expired(&mut self, tick: u64) {
        let ended: Vec<String> = self
            .ban_ends
            .iter()
            .take_while(|(end, _)| *end <= tick)
            .map(|(_, name)| name.clone())
            .collect();
        for name in ended {
            self.unban(&name);
        }
        let ended: Vec<IpAddr> = self
            .ip_ban_ends
            .iter()
            .take_while(|(end, _)| *end <= tick)
            .map(|&(_, ip)| ip)
            .collect();
        for ip in ended {
            self.unban_ip(ip);
        }
    }

    /// Builds the end indexes from the bans.
    fn index_ends(&mut self) {
        self.ban_ends = self
            .lists
            .bans
            .iter()
            .filter_map(|(name, e)| Some((e.expires()?, name.clone())))
            .collect();
        self.ip_ban_ends = self
            .lists
            .ip_bans
            .iter()
            .filter_map(|(&ip, e)| Some((e.expires()?, ip)))
            .collect();
    }

    /// Removes `name` from `kind`'s list; true if it was there.
    fn remove_name(&mut self, kind: AccessKind, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let list = if kind == AccessKind::Op {
            &self.lists.ops
        } else {
            &self.lists.whitelist
        };
        if !list.contains(&name) {
            return false;
        }
        let lists = Arc::make_mut(&mut self.lists);
        if kind == AccessKind::Op {
            &mut lists.ops
        } else {
            &mut lists.whitelist
        }
        .remove(&name);
        self.record(kind, name, AccessWrite::Delete);
        true
    }

    /// Records `write` as the last write of the entry `name` of `kind`.
    fn record(&mut self, kind: AccessKind, name: String, write: AccessWrite) {
        self.changes.insert(AccessKey { kind, name }, write);
    }
}

// A checkpoint holds the changes here and the lists beside the body ([`Access::lists`], read back
// with `set_lists`, which also builds the end indexes from the bans).
impl Codec for Access {
    fn encode(&self, w: &mut Writer) {
        let Self {
            // Kept as a shared copy beside the body.
            lists: _,
            whitelist_enabled,
            changes,
            // Derived from `bans`.
            ban_ends: _,
            // Derived from `ip_bans`.
            ip_ban_ends: _,
        } = self;
        whitelist_enabled.encode(w);
        changes.encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> checkpoint::Result<Self> {
        Ok(Self {
            lists: Arc::default(),
            whitelist_enabled: Codec::decode(r)?,
            changes: Codec::decode(r)?,
            ban_ends: BTreeSet::new(),
            ip_ban_ends: BTreeSet::new(),
        })
    }
}

/// Calls `f` with `name` in ASCII lowercase, the form the lists keep, without allocating: a name
/// already in lowercase as it is, another lowercased in a stack buffer. `None` for a name longer
/// than [`MAX_NAME_BYTES`] bytes, which no list holds.
fn folded<R>(name: &str, f: impl FnOnce(&str) -> R) -> Option<R> {
    if !name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Some(f(name));
    }
    let mut buffer = [0_u8; MAX_NAME_BYTES];
    let lower = buffer.get_mut(..name.len())?;
    lower.copy_from_slice(name.as_bytes());
    lower.make_ascii_lowercase();
    // ASCII lowercasing keeps UTF-8 valid.
    std::str::from_utf8(lower).ok().map(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_looked_up_in_any_ascii_case_without_allocating() {
        let mut access = Access::default();
        assert!(access.op("Steve").unwrap());
        assert!(access.is_op("steve") && access.is_op("STEVE") && access.is_op("sTeVe"));
        assert!(!access.is_op("alex"));
        // Longer than any listed name: not on the list, and no buffer overrun.
        assert!(!access.is_op(&"S".repeat(MAX_NAME_BYTES + 1)));
        assert!(access.whitelist_add("\u{c9}mile").unwrap());
        assert!(access.is_listed("\u{c9}MILE"), "only ASCII letters fold");
        assert_eq!(folded("AbC", str::to_owned), Some("abc".to_owned()));
    }

    /// The lists a checkpoint shares stay as they were through later changes, and put back they
    /// give the same operators and the same end indexes.
    #[test]
    fn shared_lists_keep_their_state_through_later_changes() {
        let mut access = Access::default();
        assert!(access.op("steve").unwrap());
        access
            .ban(BanEntry::new("soon").with_expires(5), 1)
            .unwrap();
        let shared = access.lists();
        assert!(access.op("alex").unwrap());
        assert!(access.unban("soon"));
        assert_eq!(shared.len(), 2, "one operator and one ban");
        let mut restored = Access::default();
        restored.set_lists(shared);
        assert!(restored.is_op("steve") && !restored.is_op("alex"));
        assert_eq!(restored.ban_ends.len(), 1);
    }

    /// A ban list change drops exactly the bans that have ended, a replaced or lifted ban's old
    /// end included, and records each as deleted.
    #[test]
    fn a_ban_list_change_drops_the_ended_bans_by_their_end() {
        let ip = IpAddr::from([10, 0, 0, 1]);
        let mut access = Access::default();
        access
            .ban(BanEntry::new("soon").with_expires(5), 1)
            .unwrap();
        access
            .ban(BanEntry::new("late").with_expires(50), 1)
            .unwrap();
        access.ban(BanEntry::new("forever"), 1).unwrap();
        access.ban(BanEntry::ip(ip).with_expires(5), 1).unwrap();
        // Replaced with a later end: the old end no longer drops it.
        access.ban(BanEntry::new("mv").with_expires(5), 1).unwrap();
        access.ban(BanEntry::new("mv").with_expires(60), 2).unwrap();
        access.take_changes();
        access.ban(BanEntry::new("new"), 10).unwrap();
        let mut changes: Vec<(String, bool)> = access
            .take_changes()
            .into_iter()
            .map(|(key, write)| (key.name, write == AccessWrite::Delete))
            .collect();
        changes.sort_unstable();
        assert_eq!(
            changes,
            [
                ("10.0.0.1".to_owned(), true),
                ("new".to_owned(), false),
                ("soon".to_owned(), true)
            ]
        );
        let names: Vec<&str> = access.bans(10).map(BanEntry::name).collect();
        assert_eq!(names, ["forever", "late", "mv", "new"]);
        assert_eq!(access.ban_ends.len(), 2, "late and mv end");
        assert!(access.unban("late"));
        assert_eq!(access.ban_ends.len(), 1);
    }
}
