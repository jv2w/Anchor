//! The operator list, the name and IP bans and the whitelist in the tick.
//!
//! Loaded from the server file at start and kept whole in memory. Every change is recorded by its
//! key until the next save snapshot takes it, so a commit writes each changed entry once, as its
//! last value: a write is absolute, and the same change twice writes the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::permission::{self, BanEntry, MAX_ENTRIES, MAX_NAME};
use crate::save::{AccessData, AccessWrite};
use crate::storage::{AccessKey, AccessKind};

/// The lists and the changes no snapshot holds yet.
#[derive(Debug, Default)]
pub(crate) struct Access {
    /// Operator names, ASCII lowercase.
    ops: BTreeSet<String>,
    /// Name bans by name (ASCII lowercase), expired ones included until a ban list change.
    bans: BTreeMap<String, BanEntry>,
    /// IP bans by address.
    ip_bans: BTreeMap<IpAddr, BanEntry>,
    /// Whitelisted names, ASCII lowercase.
    whitelist: BTreeSet<String>,
    /// Whether only whitelisted players (and operators) may join (`white-list`; not saved).
    whitelist_enabled: bool,
    /// The last write of every entry changed since the last snapshot.
    changes: BTreeMap<AccessKey, AccessWrite>,
}

crate::tick::checkpoint::fields!(Access {
    ops,
    bans,
    ip_bans,
    whitelist,
    whitelist_enabled,
    changes,
});

impl Access {
    /// The lists the server file held, with the whitelist on or off as the settings say.
    pub(crate) fn new(data: AccessData, whitelist_enabled: bool) -> Self {
        let AccessData {
            ops,
            bans,
            ip_bans,
            whitelist,
        } = data;
        Self {
            ops: ops.into_iter().collect(),
            bans: bans.into_iter().map(|e| (e.name().to_owned(), e)).collect(),
            ip_bans: ip_bans
                .into_iter()
                .filter_map(|e| Some((e.ip_address()?, e)))
                .collect(),
            whitelist: whitelist.into_iter().collect(),
            whitelist_enabled,
            changes: BTreeMap::new(),
        }
    }

    fn record(&mut self, kind: AccessKind, name: String, write: AccessWrite) {
        self.changes.insert(AccessKey { kind, name }, write);
    }

    /// Whether `name` (any ASCII case) is an operator. Asked for every permission check, so it
    /// does not allocate ([`folded`]).
    pub(super) fn is_op(&self, name: &str) -> bool {
        folded(name, |n| self.ops.contains(n)).unwrap_or(false)
    }

    /// Adds `name` to `kind`'s list (operators or whitelist); true if it was not there.
    fn add_name(&mut self, kind: AccessKind, name: &str) -> permission::Result<bool> {
        permission::check_name(name)?;
        let name = name.to_ascii_lowercase();
        let list = if kind == AccessKind::Op {
            &mut self.ops
        } else {
            &mut self.whitelist
        };
        if list.contains(&name) {
            return Ok(false);
        }
        if list.len() >= MAX_ENTRIES {
            return Err(permission::Error::Full);
        }
        list.insert(name.clone());
        self.record(kind, name, AccessWrite::Mark);
        Ok(true)
    }

    /// Removes `name` from `kind`'s list; true if it was there.
    fn remove_name(&mut self, kind: AccessKind, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let list = if kind == AccessKind::Op {
            &mut self.ops
        } else {
            &mut self.whitelist
        };
        if !list.remove(&name) {
            return false;
        }
        self.record(kind, name, AccessWrite::Delete);
        true
    }

    /// Makes `name` an operator; true if it was not one.
    pub(super) fn op(&mut self, name: &str) -> permission::Result<bool> {
        self.add_name(AccessKind::Op, name)
    }

    /// Takes operator status from `name`; true if it had it.
    pub(super) fn deop(&mut self, name: &str) -> bool {
        self.remove_name(AccessKind::Op, name)
    }

    /// The operators, ASCII lowercase, in name order.
    pub(super) fn ops(&self) -> impl Iterator<Item = &str> {
        self.ops.iter().map(String::as_str)
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
    pub(super) fn is_listed(&self, name: &str) -> bool {
        folded(name, |n| self.whitelist.contains(n)).unwrap_or(false)
    }

    /// The whitelisted names, ASCII lowercase, in name order.
    pub(super) fn whitelisted(&self) -> impl Iterator<Item = &str> {
        self.whitelist.iter().map(String::as_str)
    }

    /// Whether the whitelist is on.
    pub(super) fn whitelist_enabled(&self) -> bool {
        self.whitelist_enabled
    }

    /// Turns the whitelist on or off.
    pub(super) fn set_whitelist_enabled(&mut self, enabled: bool) {
        self.whitelist_enabled = enabled;
    }

    /// Adds or replaces a ban made at engine tick `tick`, after dropping the bans of its list
    /// that have ended (as PMMP does when a ban list changes).
    pub(super) fn ban(&mut self, entry: BanEntry, tick: u64) -> permission::Result<()> {
        let entry = entry.checked(tick)?;
        self.drop_expired(tick);
        if let Some(ip) = entry.ip_address() {
            if !self.ip_bans.contains_key(&ip) && self.ip_bans.len() >= MAX_ENTRIES {
                return Err(permission::Error::Full);
            }
            self.record(
                AccessKind::IpBan,
                entry.name().to_owned(),
                AccessWrite::Ban(entry.clone()),
            );
            self.ip_bans.insert(ip, entry);
        } else {
            if !self.bans.contains_key(entry.name()) && self.bans.len() >= MAX_ENTRIES {
                return Err(permission::Error::Full);
            }
            let name = entry.name().to_owned();
            self.record(
                AccessKind::Ban,
                name.clone(),
                AccessWrite::Ban(entry.clone()),
            );
            self.bans.insert(name, entry);
        }
        Ok(())
    }

    /// Lifts the ban of `name`; true if there was one, ended or not.
    pub(super) fn pardon(&mut self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        if self.bans.remove(&name).is_none() {
            return false;
        }
        self.record(AccessKind::Ban, name, AccessWrite::Delete);
        true
    }

    /// Lifts the ban of `ip`; true if there was one, ended or not.
    pub(super) fn pardon_ip(&mut self, ip: IpAddr) -> bool {
        if self.ip_bans.remove(&ip).is_none() {
            return false;
        }
        self.record(AccessKind::IpBan, ip.to_string(), AccessWrite::Delete);
        true
    }

    /// Drops the bans that have ended at `tick`.
    fn drop_expired(&mut self, tick: u64) {
        let ended: Vec<String> = self
            .bans
            .iter()
            .filter(|(_, e)| e.has_expired(tick))
            .map(|(n, _)| n.clone())
            .collect();
        for name in ended {
            self.pardon(&name);
        }
        let ended: Vec<IpAddr> = self
            .ip_bans
            .iter()
            .filter(|(_, e)| e.has_expired(tick))
            .map(|(ip, _)| *ip)
            .collect();
        for ip in ended {
            self.pardon_ip(ip);
        }
    }

    /// The ban of `name` (any ASCII case) in force at `tick`.
    pub(super) fn ban_entry(&self, name: &str, tick: u64) -> Option<&BanEntry> {
        folded(name, |n| self.bans.get(n))
            .flatten()
            .filter(|e| !e.has_expired(tick))
    }

    /// The ban of `ip` in force at `tick`.
    pub(super) fn ip_ban_entry(&self, ip: IpAddr, tick: u64) -> Option<&BanEntry> {
        self.ip_bans.get(&ip).filter(|e| !e.has_expired(tick))
    }

    /// The name bans in force at `tick`, in name order.
    pub(super) fn bans(&self, tick: u64) -> impl Iterator<Item = &BanEntry> {
        self.bans.values().filter(move |e| !e.has_expired(tick))
    }

    /// The IP bans in force at `tick`, in address order.
    pub(super) fn ip_bans(&self, tick: u64) -> impl Iterator<Item = &BanEntry> {
        self.ip_bans.values().filter(move |e| !e.has_expired(tick))
    }

    /// The changes since the last snapshot, each entry once with its last value, in key order.
    pub(super) fn take_changes(&mut self) -> Vec<(AccessKey, AccessWrite)> {
        std::mem::take(&mut self.changes).into_iter().collect()
    }

    /// Hashes every field (state hash).
    #[cfg(test)]
    pub(super) fn hash_into(&self, h: &mut crate::tick::Fnv) {
        let Self {
            ops,
            bans,
            ip_bans,
            whitelist,
            whitelist_enabled,
            changes,
        } = self;
        let ban = |h: &mut crate::tick::Fnv, e: &BanEntry| {
            h.bytes(e.name().as_bytes());
            h.bytes(e.reason().as_bytes());
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
}

/// `f` of `name` in ASCII lowercase, the form the lists keep, without allocating: a name already
/// in lowercase as it is, another lowercased in a stack buffer. `None` for a name longer than
/// [`MAX_NAME`] bytes, which no list holds.
fn folded<R>(name: &str, f: impl FnOnce(&str) -> R) -> Option<R> {
    if !name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Some(f(name));
    }
    let mut buffer = [0_u8; MAX_NAME];
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
        assert!(!access.is_op(&"S".repeat(MAX_NAME + 1)));
        assert!(access.whitelist_add("\u{c9}mile").unwrap());
        assert!(access.is_listed("\u{c9}MILE"), "only ASCII letters fold");
        assert_eq!(folded("AbC", str::to_owned), Some("abc".to_owned()));
    }
}
