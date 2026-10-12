//! Run-state checkpoints: the whole [`State`] at a save snapshot or at the start of the process,
//! except the raw chunk extras, which the data files of the same tick (or a generator) gives back
//! as they were stored, so that a replay can start from a committed state of a crash log.
//!
//! [`State::checkpoint`] names every field of the state, as `State::hash` does, so a new field
//! does not compile until it is kept or left out here. Each rule module encodes its own state
//! next to its `hash_into` with [`Codec`]: [`fields!`] and [`variants!`] name every field and
//! variant, so their encoders fail to compile in the same way. Encoding never fails: a value that
//! has no checkpoint form (a closure, a user's untyped extension) makes the checkpoint opaque, and
//! a replay refuses an opaque checkpoint instead of guessing.
//!
//! Light and terrain are kept as shared copies (copy-on-write light arrays and sub-chunks) and
//! encoded only when a crash log is written ([`Checkpoint::to_bytes`]), so taking a checkpoint
//! copies little on the tick. Terrain is kept although the data files holds it: a load compacts
//! the palettes, and a replay must go on from the exact chunks.
//!
//! The format: little-endian integers of fixed width, a count before every collection (u64), a
//! tag byte before every variant. Reading checks every count against the bytes left before it
//! allocates, and nesting against [`MAX_DEPTH`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::custom::Registry;
use crate::math::Vec3;
use crate::nbt::{self, Compound};
use crate::pool::Pool;
use crate::protocol::Uuid;
use crate::save::ChunkExtras;
use crate::world::{self, BlockPos, Chunk, ChunkPos, World};

use super::entity::Customs;
use super::queue::{Identity, Outbound, SessionId, Skin};
use super::task::UserData;
use super::view::Key;
use super::{Player, PlayerKey, State};

/// The deepest nesting a checkpoint reads (NBT inside NBT): as deep as NBT itself allows.
const MAX_DEPTH: usize = nbt::MAX_DEPTH;

/// A checkpoint that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    /// The bytes break a rule of the format.
    Format(&'static str),
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(what) => write!(f, "not a checkpoint: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// Where a checkpoint is written.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    out: Vec<u8>,
    /// What could not be kept, if anything: the checkpoint is opaque then.
    opaque: Option<String>,
}

impl Writer {
    pub(crate) fn into_parts(self) -> (Vec<u8>, Option<String>) {
        (self.out, self.opaque)
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    /// A collection's length.
    pub(crate) fn len(&mut self, n: usize) {
        self.raw(&u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes());
    }

    /// A slice as a [`Vec`] writes it: its length, then each item.
    pub(crate) fn slice<T: Codec>(&mut self, items: &[T]) {
        self.len(items.len());
        for v in items {
            v.encode(self);
        }
    }

    /// Bytes as a `Vec<u8>` writes them, at once.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.raw(bytes);
    }

    /// Notes something that has no checkpoint form: `count` of `what` (`count` 0 is nothing).
    pub(crate) fn opaque(&mut self, what: &str, count: usize) {
        if count > 0 && self.opaque.is_none() {
            self.opaque = Some(format!("{count} {what}"));
        }
    }
}

/// Where a checkpoint is read from, with what decoding needs: the server's custom content, which
/// saved items, blocks and custom entities are found in.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    depth: usize,
    pub(crate) custom: &'a Registry,
    pub(crate) entities: &'a Customs,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], custom: &'a Registry, entities: &'a Customs) -> Self {
        Self {
            bytes,
            depth: 0,
            custom,
            entities,
        }
    }

    /// Whether every byte was read.
    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.bytes.len() {
            return Err(Error::Format("ends early"));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Format("fixed width"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    /// A collection's length, checked against the bytes left: every element takes at least one.
    pub(crate) fn len(&mut self) -> Result<usize> {
        let n = usize::try_from(u64::from_le_bytes(self.array()?))
            .map_err(|_| Error::Format("length"))?;
        if n > self.bytes.len() {
            return Err(Error::Format("length over the bytes left"));
        }
        Ok(n)
    }

    /// Bytes as [`Writer::bytes`] wrote them.
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }

    /// Runs `read` one nesting level deeper.
    pub(crate) fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(Error::Format("nested too deep"));
        }
        self.depth += 1;
        let value = read(self);
        self.depth -= 1;
        value
    }
}

/// The room to reserve for `n` items of `T` read from `bytes_left` bytes (here and in the input
/// log): counts are checked against the bytes left, but an item in memory can be larger than its
/// smallest written form, so the reservation is bounded by the bytes left as well.
pub(crate) fn capacity<T>(n: usize, bytes_left: usize) -> usize {
    n.min(bytes_left / size_of::<T>().max(1))
}

/// A value with a checkpoint form.
pub(crate) trait Codec: Sized {
    fn encode(&self, w: &mut Writer);
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
}

/// Implements [`Codec`] for a struct field by field, naming every field: a field added later
/// does not compile until it is named here. Fields after `;` are not kept: they are empty
/// between ticks, and read back as their default.
macro_rules! fields {
    ($ty:ty { $($field:ident),* $(,)? $(; $($skip:ident),* $(,)?)? }) => {
        impl $crate::tick::checkpoint::Codec for $ty {
            fn encode(&self, w: &mut $crate::tick::checkpoint::Writer) {
                let Self { $($field,)* $($($skip: _,)*)? } = self;
                $( $crate::tick::checkpoint::Codec::encode($field, w); )*
            }

            fn decode(
                r: &mut $crate::tick::checkpoint::Reader<'_>,
            ) -> $crate::tick::checkpoint::Result<Self> {
                Ok(Self {
                    $($field: $crate::tick::checkpoint::Codec::decode(r)?,)*
                    $($($skip: Default::default(),)*)?
                })
            }
        }
    };
}
pub(crate) use fields;

/// Implements [`Codec`] for an enum: a tag byte per variant, then the variant's fields in order.
/// Every variant is named, so a variant added later does not compile until it has a tag.
macro_rules! variants {
    ($ty:ty {
        $($tag:literal => $variant:ident
            $( ( $($tuple:ident),* $(,)? ) )?
            $( { $($named:ident),* $(,)? } )?
        ),* $(,)?
    }) => {
        impl $crate::tick::checkpoint::Codec for $ty {
            fn encode(&self, w: &mut $crate::tick::checkpoint::Writer) {
                match self {
                    $(
                        Self::$variant $( ( $($tuple),* ) )? $( { $($named),* } )? => {
                            w.u8($tag);
                            $( $( $crate::tick::checkpoint::Codec::encode($tuple, w); )* )?
                            $( $( $crate::tick::checkpoint::Codec::encode($named, w); )* )?
                        }
                    )*
                }
            }

            fn decode(
                r: &mut $crate::tick::checkpoint::Reader<'_>,
            ) -> $crate::tick::checkpoint::Result<Self> {
                Ok(match r.u8()? {
                    $(
                        $tag => Self::$variant
                            $( ( $( $crate::tick::checkpoint::variants!(@field r $tuple) ),* ) )?
                            $( { $( $named: $crate::tick::checkpoint::Codec::decode(r)? ),* } )?,
                    )*
                    _ => {
                        return Err($crate::tick::checkpoint::Error::Format(concat!(
                            "variant of ",
                            stringify!($ty)
                        )))
                    }
                })
            }
        }
    };
    (@field $r:ident $name:ident) => {
        $crate::tick::checkpoint::Codec::decode($r)?
    };
}
pub(crate) use variants;

macro_rules! integers {
    ($($ty:ty),*) => {
        $(
            impl Codec for $ty {
                fn encode(&self, w: &mut Writer) {
                    w.raw(&self.to_le_bytes());
                }

                fn decode(r: &mut Reader<'_>) -> Result<Self> {
                    Ok(Self::from_le_bytes(r.array()?))
                }
            }
        )*
    };
}

integers!(u8, u16, u32, u64, i8, i16, i32, i64);

impl Codec for usize {
    fn encode(&self, w: &mut Writer) {
        u64::try_from(*self).unwrap_or(u64::MAX).encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::try_from(u64::decode(r)?).map_err(|_| Error::Format("usize"))
    }
}

impl Codec for bool {
    fn encode(&self, w: &mut Writer) {
        w.u8(u8::from(*self));
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Format("bool")),
        }
    }
}

impl Codec for f32 {
    fn encode(&self, w: &mut Writer) {
        self.to_bits().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::from_bits(u32::decode(r)?))
    }
}

impl Codec for f64 {
    fn encode(&self, w: &mut Writer) {
        self.to_bits().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::from_bits(u64::decode(r)?))
    }
}

impl Codec for NonZeroU32 {
    fn encode(&self, w: &mut Writer) {
        self.get().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::new(u32::decode(r)?).ok_or(Error::Format("zero"))
    }
}

impl Codec for String {
    fn encode(&self, w: &mut Writer) {
        w.len(self.len());
        w.raw(self.as_bytes());
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.len()?;
        Self::from_utf8(r.take(n)?.to_vec()).map_err(|_| Error::Format("text"))
    }
}

impl<T: Codec> Codec for Vec<T> {
    fn encode(&self, w: &mut Writer) {
        w.slice(self);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.len()?;
        let mut out = Self::with_capacity(capacity::<T>(n, r.bytes.len()));
        for _ in 0..n {
            out.push(T::decode(r)?);
        }
        Ok(out)
    }
}

impl<T: Codec> Codec for VecDeque<T> {
    fn encode(&self, w: &mut Writer) {
        w.len(self.len());
        for v in self {
            v.encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Vec::decode(r)?.into())
    }
}

impl<T: Codec + Ord> Codec for BTreeSet<T> {
    fn encode(&self, w: &mut Writer) {
        w.len(self.len());
        for v in self {
            v.encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.len()?;
        let mut out = Self::new();
        for _ in 0..n {
            if !out.insert(T::decode(r)?) {
                return Err(Error::Format("repeated set entry"));
            }
        }
        Ok(out)
    }
}

impl<K: Codec + Ord, V: Codec> Codec for BTreeMap<K, V> {
    fn encode(&self, w: &mut Writer) {
        w.len(self.len());
        for (k, v) in self {
            k.encode(w);
            v.encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.len()?;
        let mut out = Self::new();
        for _ in 0..n {
            let k = K::decode(r)?;
            if out.insert(k, V::decode(r)?).is_some() {
                return Err(Error::Format("repeated map key"));
            }
        }
        Ok(out)
    }
}

impl<T: Codec> Codec for Option<T> {
    fn encode(&self, w: &mut Writer) {
        match self {
            None => w.u8(0),
            Some(v) => {
                w.u8(1);
                v.encode(w);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(r)?)),
            _ => Err(Error::Format("option")),
        }
    }
}

impl<T: Codec> Codec for Box<T> {
    fn encode(&self, w: &mut Writer) {
        (**self).encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(T::decode(r)?))
    }
}

impl<T: Codec> Codec for Arc<T> {
    fn encode(&self, w: &mut Writer) {
        (**self).encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(T::decode(r)?))
    }
}

impl<T: Codec, const N: usize> Codec for [T; N] {
    fn encode(&self, w: &mut Writer) {
        for v in self {
            v.encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let mut out = Vec::with_capacity(N);
        for _ in 0..N {
            out.push(T::decode(r)?);
        }
        out.try_into().map_err(|_| Error::Format("array"))
    }
}

macro_rules! tuples {
    ($(($($name:ident),+)),*) => {
        $(
            impl<$($name: Codec),+> Codec for ($($name,)+) {
                #[expect(non_snake_case, reason = "consistency: the type parameters name the parts")]
                fn encode(&self, w: &mut Writer) {
                    let ($($name,)+) = self;
                    $( $name.encode(w); )+
                }

                fn decode(r: &mut Reader<'_>) -> Result<Self> {
                    Ok(($($name::decode(r)?,)+))
                }
            }
        )*
    };
}

tuples!((A, B), (A, B, C), (A, B, C, D));

fields!(Vec3 { x, y, z });
fields!(BlockPos { x, y, z });
fields!(ChunkPos { x, z });
fields!(crate::math::Aabb { min, max });

impl Codec for Uuid {
    fn encode(&self, w: &mut Writer) {
        w.raw(self.as_bytes());
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::from_bytes(r.array()?))
    }
}

variants!(crate::crafting::FurnaceType {
    0 => Furnace,
    1 => BlastFurnace,
    2 => Smoker,
    3 => Campfire,
    4 => SoulCampfire,
});

variants!(crate::menu::Kind {
    0 => Chest,
    1 => DoubleChest,
    2 => Hopper,
});

variants!(crate::config::GameMode {
    0 => Survival,
    1 => Creative,
    2 => Adventure,
    3 => Spectator,
});

variants!(crate::config::Difficulty {
    0 => Peaceful,
    1 => Easy,
    2 => Normal,
    3 => Hard,
});

variants!(crate::world::Weather {
    0 => Clear,
    1 => Rain,
    2 => Thunder,
});

variants!(crate::event::DamageCause {
    0 => Contact,
    1 => EntityAttack,
    2 => Projectile,
    3 => Suffocation,
    4 => Fall,
    5 => Fire,
    6 => FireTick,
    7 => Lava,
    8 => Drowning,
    9 => BlockExplosion,
    10 => EntityExplosion,
    11 => Void,
    12 => Suicide,
    13 => Magic,
    14 => Custom,
    15 => Starvation,
    16 => FallingBlock,
});

variants!(crate::event::DamageSource {
    0 => None,
    1 => Block(pos),
    2 => Entity(actor),
    3 => Child { child, owner },
});

variants!(crate::event::Actor {
    0 => Player(id),
    1 => Entity(id),
});

impl Codec for crate::permission::BanEntry {
    fn encode(&self, w: &mut Writer) {
        self.name().to_owned().encode(w);
        self.ip_address().encode(w);
        self.reason().to_owned().encode(w);
        self.source().to_owned().encode(w);
        self.created().encode(w);
        self.expires().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::from_parts(
            Codec::decode(r)?,
            Codec::decode(r)?,
            Codec::decode(r)?,
            Codec::decode(r)?,
            Codec::decode(r)?,
            Codec::decode(r)?,
        ))
    }
}

fields!(crate::save::PlayerData {
    world,
    position,
    extra
});

impl Codec for crate::storage::PlayerKey {
    fn encode(&self, w: &mut Writer) {
        self.as_str().to_owned().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::from_text(&String::decode(r)?))
    }
}

variants!(crate::storage::AccessKind {
    0 => Op,
    1 => Ban,
    2 => IpBan,
    3 => Whitelist,
});
fields!(crate::storage::AccessKey { kind, name });
variants!(crate::save::AccessWrite {
    0 => Delete,
    1 => Mark,
    2 => Ban(entry),
});

variants!(crate::display::Colour {
    0 => Pink,
    1 => Blue,
    2 => Red,
    3 => Green,
    4 => Yellow,
    5 => Purple,
    6 => RebeccaPurple,
    7 => White,
});
variants!(crate::display::Sort {
    0 => Ascending,
    1 => Descending,
});
fields!(crate::display::BossBar {
    title,
    progress,
    colour
});

variants!(crate::entity::ArrowPickup {
    0 => None,
    1 => Any,
    2 => Creative,
});

impl Codec for crate::entity::PaintingMotive {
    fn encode(&self, w: &mut Writer) {
        self.name().to_owned().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_name(&String::decode(r)?).ok_or(Error::Format("painting motive"))
    }
}

/// A human NPC's skin, checked again as it is made.
impl Codec for crate::entity::Skin {
    fn encode(&self, w: &mut Writer) {
        self.id().to_owned().encode(w);
        self.data().to_vec().encode(w);
        self.cape().to_vec().encode(w);
        self.geometry_name().to_owned().encode(w);
        self.geometry_data().to_vec().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let id = String::decode(r)?;
        let data = Vec::decode(r)?;
        let cape = Vec::decode(r)?;
        let geometry_name = String::decode(r)?;
        let geometry_data = Vec::decode(r)?;
        Self::new(id, data)
            .and_then(|s| s.with_cape(cape))
            .and_then(|s| s.with_geometry(geometry_name, geometry_data))
            .ok_or(Error::Format("skin"))
    }
}

variants!(crate::block::Facing {
    0 => North,
    1 => East,
    2 => South,
    3 => West,
});

impl Codec for crate::block::Face {
    fn encode(&self, w: &mut Writer) {
        self.index().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_index(i32::from(u8::decode(r)?)).ok_or(Error::Format("face"))
    }
}

impl Codec for crate::player::Id {
    fn encode(&self, w: &mut Writer) {
        self.session().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(SessionId::decode(r)?))
    }
}

impl Codec for crate::effect::Type {
    fn encode(&self, w: &mut Writer) {
        self.id().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_id(i32::from(u8::decode(r)?)).ok_or(Error::Format("effect"))
    }
}

impl Codec for crate::effect::Potion {
    fn encode(&self, w: &mut Writer) {
        self.id().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_id(i32::from(u8::decode(r)?)).ok_or(Error::Format("potion"))
    }
}

impl Codec for crate::effect::Instance {
    fn encode(&self, w: &mut Writer) {
        self.effect().encode(w);
        self.duration_ticks().encode(w);
        self.amplifier().encode(w);
        self.is_visible().encode(w);
        self.is_ambient().encode(w);
        self.is_infinite().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let effect = Codec::decode(r)?;
        let duration = Codec::decode(r)?;
        let amplifier = Codec::decode(r)?;
        let visible = Codec::decode(r)?;
        let ambient = Codec::decode(r)?;
        let infinite = Codec::decode(r)?;
        Ok(Self::new(effect, duration, amplifier, visible, ambient).with_infinite(infinite))
    }
}

impl<F: crate::protocol::Flag> Codec for crate::protocol::FlagSet<F> {
    fn encode(&self, w: &mut Writer) {
        self.words().to_vec().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let words: Vec<u64> = Vec::decode(r)?;
        Self::from_words(&words).map_err(|_| Error::Format("flags"))
    }
}

impl Codec for crate::entity::Id {
    fn encode(&self, w: &mut Writer) {
        self.index().encode(w);
        self.generation().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(u32::decode(r)?, u32::decode(r)?))
    }
}

impl Codec for crate::menu::Id {
    fn encode(&self, w: &mut Writer) {
        self.get().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(u32::decode(r)?))
    }
}

impl Codec for crate::item::Enchantment {
    fn encode(&self, w: &mut Writer) {
        self.id().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_id(i16::from(u8::decode(r)?)).ok_or(Error::Format("enchantment"))
    }
}

impl Codec for SessionId {
    fn encode(&self, w: &mut Writer) {
        self.get().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self::new(u64::decode(r)?))
    }
}

impl Codec for crate::protocol::consts::InputMode {
    fn encode(&self, w: &mut Writer) {
        u32::from(*self).encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::try_from(u32::decode(r)?).map_err(|_| Error::Format("input mode"))
    }
}

impl Codec for IpAddr {
    fn encode(&self, w: &mut Writer) {
        match self {
            Self::V4(a) => {
                w.u8(4);
                w.raw(&a.octets());
            }
            Self::V6(a) => {
                w.u8(6);
                w.raw(&a.octets());
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        match r.u8()? {
            4 => Ok(Self::from(r.array::<4>()?)),
            6 => Ok(Self::from(r.array::<16>()?)),
            _ => Err(Error::Format("address")),
        }
    }
}

/// A skin in its protocol form.
impl Codec for Skin {
    fn encode(&self, w: &mut Writer) {
        let mut bytes = crate::protocol::Writer::new(Vec::new(), 0);
        if crate::protocol::Codec::encode(self.get(), &mut bytes).is_ok() {
            w.bytes(bytes.as_bytes());
        } else {
            w.opaque("skin that does not encode", 1);
            w.len(0);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.len()?;
        let bytes = r.take(n)?;
        let mut reader = crate::protocol::Reader::new(bytes, bytes.len().saturating_mul(2), 0);
        let skin = <crate::protocol::Skin as crate::protocol::Codec>::decode(&mut reader)
            .map_err(|_| Error::Format("skin"))?;
        if !reader.remaining().is_empty() {
            return Err(Error::Format("bytes after a skin"));
        }
        Ok(Self::new(skin))
    }
}

/// A block state: a vanilla state by its network ID, a custom one by its saved NBT, found in the
/// server's custom content.
impl Codec for crate::block::State {
    fn encode(&self, w: &mut Writer) {
        if self.custom_block().is_some() {
            w.u8(1);
            crate::save::state_nbt(*self).encode(w);
        } else {
            w.u8(0);
            self.network_id().encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Self::from_network_id(u32::decode(r)?).ok_or(Error::Format("block state")),
            1 => {
                let nbt = Compound::decode(r)?;
                r.custom
                    .find_state_nbt(&nbt)
                    .ok_or(Error::Format("custom block state"))
            }
            _ => Err(Error::Format("block state kind")),
        }
    }
}

/// An inert block state by its network ID.
impl Codec for crate::block::Inert {
    fn encode(&self, w: &mut Writer) {
        self.network_id().encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Self::from_network_id(u32::decode(r)?).ok_or(Error::Format("inert block state"))
    }
}

/// A stack by its disk form, which reads back equal ([`crate::item::Stack::to_nbt`]).
impl Codec for crate::item::Stack {
    fn encode(&self, w: &mut Writer) {
        if self.is_empty() {
            w.u8(0);
        } else {
            w.u8(1);
            self.to_nbt().encode(w);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Self::EMPTY),
            1 => {
                let nbt = Compound::decode(r)?;
                match Self::from_nbt_with(&nbt, r.custom) {
                    Some(stack) if !stack.is_empty() => Ok(stack),
                    _ => Err(Error::Format("item stack")),
                }
            }
            _ => Err(Error::Format("item stack kind")),
        }
    }
}

/// An item type as the stack of one of it.
impl Codec for crate::item::Type {
    fn encode(&self, w: &mut Writer) {
        crate::item::Stack::new(*self, 1).encode(w);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        crate::item::Stack::decode(r)?
            .item()
            .ok_or(Error::Format("item type"))
    }
}

/// Implements [`Codec`] for an NBT enum: a tag byte per variant; the variants that hold
/// compounds or lists read one nesting level deeper.
macro_rules! nbt_codec {
    ($ty:ty, $what:literal {
        $($tag:literal => $variant:ident $( ( $flat:ident ) )? $( [ $deep:ident ] )?),* $(,)?
    }) => {
        impl Codec for $ty {
            fn encode(&self, w: &mut Writer) {
                match self {
                    $(
                        Self::$variant $( ( $flat ) )? $( ( $deep ) )? => {
                            w.u8($tag);
                            $( $flat.encode(w); )?
                            $( $deep.encode(w); )?
                        }
                    )*
                }
            }

            fn decode(r: &mut Reader<'_>) -> Result<Self> {
                Ok(match r.u8()? {
                    $(
                        $tag => Self::$variant
                            $( ( nbt_codec!(@flat r $flat) ) )?
                            $( ( nbt_codec!(@deep r $deep) ) )?,
                    )*
                    _ => return Err(Error::Format($what)),
                })
            }
        }
    };
    (@flat $r:ident $v:ident) => {
        Codec::decode($r)?
    };
    (@deep $r:ident $v:ident) => {
        $r.nested(Codec::decode)?
    };
}

nbt_codec!(nbt::Value, "nbt value" {
    0 => Byte(v),
    1 => Short(v),
    2 => Int(v),
    3 => Long(v),
    4 => Float(v),
    5 => Double(v),
    6 => ByteArray(v),
    7 => String(v),
    8 => List[v],
    9 => Compound[v],
    10 => IntArray(v),
    11 => LongArray(v),
});

nbt_codec!(nbt::List, "nbt list" {
    0 => End,
    1 => Byte(v),
    2 => Short(v),
    3 => Int(v),
    4 => Long(v),
    5 => Float(v),
    6 => Double(v),
    7 => ByteArray(v),
    8 => String(v),
    9 => List[v],
    10 => Compound[v],
    11 => IntArray(v),
    12 => LongArray(v),
});

variants!(super::Phase {
    0 => Loading,
    1 => Joined,
    2 => Spawning,
    3 => Spawned,
});

/// What a restore reads the chunks with: the terrain the checkpoint kept of the unloaded chunks
/// waiting for a commit, and the raw extras of every chunk it names, which the data files or the
/// generator gives ([`Checkpoint::loaded`], [`Checkpoint::unsaved`]).
#[derive(Debug, Default)]
pub(crate) struct Ground {
    pub(crate) chunks: BTreeMap<Key, Chunk>,
    pub(crate) extras: BTreeMap<Key, ChunkExtras>,
}

/// The state at a tick boundary, as a replay starts from it ([`State::checkpoint`]).
#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    /// The engine tick of the state: the first tick a replay from it applies.
    tick: u64,
    /// The worlds the server had, loaded or not, by world index: a replay makes the same ones.
    worlds: Vec<super::world_api::WorldSlot>,
    /// Each world's plain fields, by world index.
    saved: Vec<world::Saved>,
    /// The rest of the state, encoded.
    body: Vec<u8>,
    /// Each world's light work and loaded chunks, by world index, and the unloaded chunks
    /// waiting for a commit: shared with the state until they change, encoded when a crash log
    /// is written.
    kept: Vec<world::Kept>,
    unsaved: Vec<(Key, Chunk)>,
    /// What has no checkpoint form, if anything: a replay refuses the checkpoint then.
    opaque: Option<String>,
}

fields!(Checkpoint {
    tick,
    worlds,
    saved,
    body,
    kept,
    unsaved,
    opaque,
});

impl Checkpoint {
    /// The engine tick of the state.
    pub(crate) fn tick(&self) -> u64 {
        self.tick
    }

    /// What has no checkpoint form, if anything.
    pub(crate) fn opaque(&self) -> Option<&str> {
        self.opaque.as_deref()
    }

    /// The seed of each world, by world index.
    pub(crate) fn seeds(&self) -> impl Iterator<Item = i64> + '_ {
        self.saved.iter().map(|s| s.seed)
    }

    /// The worlds the server had, loaded or not, by world index.
    pub(crate) fn worlds(&self) -> &[super::world_api::WorldSlot] {
        &self.worlds
    }

    /// The loaded chunks, whose terrain and extras a replay reads from the data files.
    pub(crate) fn loaded(&self) -> impl Iterator<Item = Key> + '_ {
        (0..)
            .zip(&self.saved)
            .flat_map(|(w, s)| s.loaded.iter().map(move |&p| (w, p)))
    }

    /// The unloaded chunks waiting for a commit, whose extras a replay reads from the data files.
    pub(crate) fn unsaved(&self) -> impl Iterator<Item = Key> + '_ {
        self.unsaved.iter().map(|(k, _)| *k)
    }

    /// The bytes it holds or will encode to, about: what the input log charges for it.
    pub(crate) fn size(&self) -> usize {
        let kept: usize = self.kept.iter().map(world::Kept::size).sum();
        let positions: usize = self.saved.iter().map(|s| s.loaded.len()).sum();
        let unsaved = self
            .unsaved
            .iter()
            .map(|(_, c)| usize::from(c.sub_chunk_limit()).saturating_mul(4 << 10))
            .fold(0, usize::saturating_add);
        self.body
            .len()
            .saturating_add(kept)
            .saturating_add(unsaved)
            .saturating_add(positions.saturating_mul(16))
    }

    /// The checkpoint's written form.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        self.encode(&mut w);
        w.out
    }

    /// Reads a written checkpoint with the server's custom content: its items, blocks and
    /// entity types.
    pub(crate) fn read(bytes: &[u8], custom: &Registry, entities: &Customs) -> Result<Self> {
        let mut r = Reader::new(bytes, custom, entities);
        let checkpoint = Self::decode(&mut r)?;
        if r.is_empty() {
            Ok(checkpoint)
        } else {
            Err(Error::Format("bytes after the checkpoint"))
        }
    }
}

/// A player as the body keeps it: every field but the session (the map's key), the key (its
/// identity's again), the outbound queue (a replay gives a new one) and what is empty between
/// ticks (the output, the outboxes and the overflow mark, whose player is gone by then).
fn encode_player(p: &Player, data: &UserData, w: &mut Writer) {
    let Player {
        session: _,
        runtime_id,
        identity,
        key: _,
        world,
        position,
        data_dirty,
        chunk_radius,
        asked_radius,
        phase,
        view,
        outbound: _,
        pending: _,
        extra,
        inventory,
        survival,
        breaking,
        outbox: _,
        shared_outbox: _,
        overflowed: _,
        marks,
        presence,
    } = p;
    runtime_id.encode(w);
    identity.encode(w);
    world.encode(w);
    position.encode(w);
    data_dirty.encode(w);
    chunk_radius.encode(w);
    asked_radius.encode(w);
    phase.encode(w);
    view.encode(w);
    extra.checkpoint(data, w);
    inventory.encode(w);
    survival.encode(w);
    breaking.encode(w);
    marks.encode(w);
    presence.encode(w);
}

/// Reads what [`encode_player`] wrote for `session`, with the outbound queue `outbound`.
fn decode_player(
    r: &mut Reader<'_>,
    session: SessionId,
    data: &UserData,
    outbound: mpsc::Sender<Outbound>,
) -> Result<Player> {
    let runtime_id = Codec::decode(r)?;
    let identity: Identity = Codec::decode(r)?;
    let key = PlayerKey::of(&identity);
    Ok(Player {
        session,
        runtime_id,
        identity,
        key,
        world: Codec::decode(r)?,
        position: Codec::decode(r)?,
        data_dirty: Codec::decode(r)?,
        chunk_radius: Codec::decode(r)?,
        asked_radius: Codec::decode(r)?,
        phase: Codec::decode(r)?,
        view: Codec::decode(r)?,
        outbound,
        pending: super::spawn::Pending::default(),
        extra: super::extra::PlayerExtra::restore(r, data)?,
        inventory: Codec::decode(r)?,
        survival: Codec::decode(r)?,
        breaking: Codec::decode(r)?,
        outbox: Vec::new(),
        shared_outbox: Vec::new(),
        overflowed: false,
        marks: Codec::decode(r)?,
        presence: Codec::decode(r)?,
    })
}

impl State {
    /// The state as a replay starts from it. Every field is named, as in `State::hash`: the indexes
    /// are functions of the players; the listeners, hooks, shared batches and custom content are
    /// fixed at build or start; the settings no API call changes are in the log's settings
    /// fingerprint. Players are encoded on the CPU pool.
    pub(crate) fn checkpoint(&self, pool: &Pool) -> Checkpoint {
        let Self {
            settings,
            shared: _,
            tick,
            next_runtime_id,
            players,
            index: _,
            names: _,
            uuids: _,
            listeners: _,
            tasks,
            data,
            commands,
            hooks: _,
            forms,
            menus,
            deferred,
            worlds,
            catalog,
            chunks,
            saving,
            loading,
            runtime_ids: _,
            rules,
            clock,
            trades,
            custom: _,
            fingerprints: _,
            identity: _,
            network,
            awake: _,
        } = self;
        let mut w = Writer::default();
        settings.game_mode.encode(&mut w);
        settings.difficulty.encode(&mut w);
        settings.max_players.encode(&mut w);
        settings.random_tick_speed.encode(&mut w);
        settings.motd.encode(&mut w);
        settings.world_views.encode(&mut w);
        next_runtime_id.encode(&mut w);
        let list: Vec<(&SessionId, &Player)> = players.iter().collect();
        let encoded = pool.map(&list, |(_, p)| {
            let mut pw = Writer::default();
            encode_player(p, data, &mut pw);
            pw.into_parts()
        });
        w.len(encoded.len());
        for ((session, _), (bytes, opaque)) in list.iter().zip(encoded) {
            session.encode(&mut w);
            w.bytes(&bytes);
            if let Some(what) = opaque {
                w.opaque(&what, 1);
            }
        }
        tasks.checkpoint(&mut w);
        data.checkpoint(&mut w);
        commands.checkpoint(&mut w);
        forms.encode(&mut w);
        w.opaque("open menus", menus.len());
        w.opaque("deferred callbacks", deferred.len());
        chunks.checkpoint(&mut w);
        saving.encode(&mut w);
        loading.encode(&mut w);
        rules.checkpoint(&mut w);
        clock.encode(&mut w);
        trades.encode(&mut w);
        catalog.checkpoint(&mut w);
        network.encode(&mut w);
        let (saved, kept) = worlds.iter().map(World::checkpoint).unzip();
        let (body, opaque) = w.into_parts();
        Checkpoint {
            tick: *tick,
            worlds: catalog.slots(worlds),
            saved,
            body,
            kept,
            unsaved: chunks
                .unsaved
                .iter()
                .map(|(&key, u)| (key, u.chunk.clone()))
                .collect(),
            opaque,
        }
    }

    /// Puts the state of `checkpoint` into this state, which [`State::new`] made at its tick from
    /// the same build (listeners, tasks, commands, custom content) and worlds. `extras` gives the
    /// raw extras the data files or the generator has for the chunks the checkpoint names;
    /// `outbound` gives each player a new outbound queue.
    pub(crate) fn restore(
        &mut self,
        checkpoint: &Checkpoint,
        extras: BTreeMap<Key, ChunkExtras>,
        mut outbound: impl FnMut(SessionId) -> mpsc::Sender<Outbound>,
    ) -> Result<()> {
        let Checkpoint {
            tick,
            worlds,
            saved,
            body,
            kept,
            unsaved,
            opaque,
        } = checkpoint;
        if opaque.is_some() {
            return Err(Error::Format("an opaque checkpoint"));
        }
        let n = self.worlds.len();
        if worlds.len() != n || saved.len() != n || kept.len() != n {
            return Err(Error::Format("world count"));
        }
        if !self
            .worlds
            .iter()
            .zip(worlds)
            .all(|(w, s)| w.name() == s.name && w.kind() == s.kind)
        {
            return Err(Error::Format("worlds"));
        }
        let mut ground = Ground {
            chunks: unsaved.iter().cloned().collect(),
            extras,
        };
        let custom = Arc::clone(&self.custom);
        let entities = self.rules.entities.take_customs();
        let result = self.restore_body(
            *tick,
            body,
            worlds,
            &mut ground,
            &mut outbound,
            &custom,
            &entities,
        );
        self.rules.entities.set_customs(entities);
        result?;
        for ((world, saved), kept) in self.worlds.iter_mut().zip(saved).zip(kept) {
            world
                .restore(saved.clone(), kept.clone())
                .map_err(|_| Error::Format("world"))?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "consistency: the parts of a checkpoint the body reads with, each named"
    )]
    fn restore_body(
        &mut self,
        tick: u64,
        body: &[u8],
        worlds: &[super::world_api::WorldSlot],
        ground: &mut Ground,
        outbound: &mut impl FnMut(SessionId) -> mpsc::Sender<Outbound>,
        custom: &Registry,
        entities: &Customs,
    ) -> Result<()> {
        let mut r = Reader::new(body, custom, entities);
        self.tick = tick;
        self.settings.game_mode = Codec::decode(&mut r)?;
        self.settings.difficulty = Codec::decode(&mut r)?;
        self.settings.max_players = Codec::decode(&mut r)?;
        self.settings.random_tick_speed = Codec::decode(&mut r)?;
        self.settings.motd = Codec::decode(&mut r)?;
        self.settings.world_views = Codec::decode(&mut r)?;
        self.next_runtime_id = Codec::decode(&mut r)?;
        let n = r.len()?;
        let mut players = Vec::with_capacity(capacity::<(SessionId, Player)>(n, r.bytes.len()));
        for _ in 0..n {
            let session = SessionId::decode(&mut r)?;
            let bytes = r.bytes()?;
            let mut pr = Reader::new(bytes, custom, entities);
            let player = decode_player(&mut pr, session, &self.data, outbound(session))?;
            if !pr.is_empty() {
                return Err(Error::Format("bytes after a player"));
            }
            players.push((session, player));
        }
        self.tasks.restore(&mut r)?;
        self.data.restore(&mut r)?;
        for (session, player) in players {
            if self.players.contains_key(&session) || self.index.contains_key(&player.key) {
                return Err(Error::Format("repeated player"));
            }
            self.insert(session, player);
        }
        self.commands.restore(&mut r)?;
        self.forms = Codec::decode(&mut r)?;
        self.chunks.restore(&mut r, ground)?;
        self.saving = Codec::decode(&mut r)?;
        self.loading = Codec::decode(&mut r)?;
        self.rules.restore(&mut r)?;
        self.clock = Codec::decode(&mut r)?;
        self.trades = Codec::decode(&mut r)?;
        self.catalog.restore(&mut r, worlds)?;
        self.network.restore(Codec::decode(&mut r)?);
        if r.is_empty() {
            Ok(())
        } else {
            Err(Error::Format("bytes after the state"))
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    //! Checkpoint round trips: the determinism runs check one at every save boundary they reach.

    use super::*;

    /// The raw extras the data files hold for `state`'s chunks right after a snapshot.
    pub(in crate::tick) fn extras_of(state: &State) -> BTreeMap<Key, ChunkExtras> {
        let mut extras = BTreeMap::new();
        for (w, world) in state.worlds.iter().enumerate() {
            for pos in world.loaded() {
                if let Some(e) = state.chunks.extras.get((w, pos)) {
                    extras.insert((w, pos), e.clone());
                }
            }
        }
        for (&key, unsaved) in &state.chunks.unsaved {
            extras.insert(key, unsaved.extras.extras().clone());
        }
        extras
    }

    /// Checks that a checkpoint of `state` reads back to the same bytes and, unless it is opaque,
    /// restores into `fresh` (a state made as `state` was, at any tick) with the same hash, the
    /// same indexes and the same checkpoint. Returns whether it was restored.
    pub(in crate::tick) fn assert_round_trip(state: &State, mut fresh: State, pool: &Pool) -> bool {
        let checkpoint = state.checkpoint(pool);
        let bytes = checkpoint.to_bytes();
        let custom = Arc::clone(&fresh.custom);
        let entities = fresh.rules.entities.take_customs();
        let read = Checkpoint::read(&bytes, &custom, &entities);
        fresh.rules.entities.set_customs(entities);
        let read = read.expect("a checkpoint reads back");
        assert_eq!(
            read.to_bytes(),
            bytes,
            "a checkpoint reads back to its bytes"
        );
        if read.opaque().is_some() {
            assert!(
                fresh
                    .restore(&read, BTreeMap::new(), |_| outbound_queue_sender())
                    .is_err(),
                "an opaque checkpoint is refused"
            );
            return false;
        }
        fresh
            .restore(&read, extras_of(state), |_| outbound_queue_sender())
            .expect("a checkpoint restores");
        assert_eq!(fresh.hash(), state.hash(), "the restored state's hash");
        assert!(fresh.index_matches_players());
        assert_eq!(
            fresh.checkpoint(pool).to_bytes(),
            bytes,
            "the restored state's checkpoint"
        );
        true
    }

    thread_local! {
        /// The process's committed memory while the last [`Big`] was decoded.
        static COMMITTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The memory the process has committed (Windows) or mapped.
    #[expect(
        clippy::disallowed_methods,
        reason = "off-tick: the test reads the process's memory"
    )]
    fn committed() -> usize {
        memory_stats::memory_stats().map_or(0, |m| m.virtual_mem)
    }

    /// An item of 16 KiB whose decoding notes the committed memory and fails.
    struct Big([u8; 16_384]);

    impl Codec for Big {
        fn encode(&self, w: &mut Writer) {
            w.raw(&self.0);
        }

        fn decode(_: &mut Reader<'_>) -> Result<Self> {
            COMMITTED.set(committed());
            Err(Error::Format("big"))
        }
    }

    #[test]
    fn a_length_of_large_items_reserves_no_more_than_the_bytes_left() {
        // 128 KiB: a length as large as the bytes left, of items of 16 KiB (2 GiB for them all).
        let mut bytes = ((1_u64 << 17) - 8).to_le_bytes().to_vec();
        bytes.resize(1 << 17, 0);
        let mut w = Writer::default();
        Big([0; 16_384]).encode(&mut w);
        let custom = Registry::default();
        let entities = Customs::default();
        let before = committed();
        let mut r = Reader::new(&bytes, &custom, &entities);
        assert!(Vec::<Big>::decode(&mut r).is_err());
        let reserved = COMMITTED.get().saturating_sub(before);
        assert!(reserved < 1 << 30, "{reserved} bytes reserved");
    }

    /// An outbound queue whose receiver is gone (nothing is delivered in a round trip).
    fn outbound_queue_sender() -> mpsc::Sender<Outbound> {
        super::super::queue::outbound_queue().0
    }
}
