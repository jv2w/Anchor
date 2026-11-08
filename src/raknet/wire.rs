//! RakNet byte formats, as go-raknet writes them (`internal/message`, `packet.go`,
//! `acknowledge.go`, `binary.go`): message IDs, offline messages, addresses, frame headers and
//! acknowledgement records.

use std::net::{IpAddr, SocketAddr};

use super::{Error, Result};

/// Address family that go-raknet writes for IPv6 addresses (`AF_INET6` on Windows).
const AF_INET6: u16 = 23;
/// RakNet ordering channels (`NUMBER_OF_ORDERED_STREAMS`).
pub(super) const CHANNELS: usize = 32;
/// Flags byte and 24-bit sequence number of a data datagram.
pub(super) const DATAGRAM_HEADER_BYTES: usize = 4;

/// Set on data datagrams by go-raknet and RakNet; receivers ignore it.
pub(super) const FLAG_NEEDS_B_AND_AS: u8 = 0x04;
/// Set on a negative acknowledgement datagram.
pub(super) const FLAG_NAK: u8 = 0x20;
/// Set on an acknowledgement datagram.
pub(super) const FLAG_ACK: u8 = 0x40;
/// Set on every connected datagram, ACK and NAK.
pub(super) const FLAG_DATAGRAM: u8 = 0x80;

/// The ID of a connected ping.
pub(super) const ID_CONNECTED_PING: u8 = 0x00;
/// The ID of an unconnected ping.
pub(super) const ID_UNCONNECTED_PING: u8 = 0x01;
/// The ID of an unconnected ping that only open connections answer.
pub(super) const ID_UNCONNECTED_PING_OPEN_CONNECTIONS: u8 = 0x02;
/// The ID of a connected pong.
pub(super) const ID_CONNECTED_PONG: u8 = 0x03;
/// The ID of the notice that a connection is lost.
pub(super) const ID_DETECT_LOST_CONNECTIONS: u8 = 0x04;
/// The ID of the first open connection request.
pub(super) const ID_OPEN_CONNECTION_REQUEST_1: u8 = 0x05;
/// The ID of the reply to the first open connection request.
pub(super) const ID_OPEN_CONNECTION_REPLY_1: u8 = 0x06;
/// The ID of the second open connection request.
pub(super) const ID_OPEN_CONNECTION_REQUEST_2: u8 = 0x07;
/// The ID of the reply to the second open connection request.
pub(super) const ID_OPEN_CONNECTION_REPLY_2: u8 = 0x08;
/// The ID of a connection request.
pub(super) const ID_CONNECTION_REQUEST: u8 = 0x09;
/// The ID of the acceptance of a connection request.
pub(super) const ID_CONNECTION_REQUEST_ACCEPTED: u8 = 0x10;
/// The ID of the notice that a new connection came in.
pub(super) const ID_NEW_INCOMING_CONNECTION: u8 = 0x13;
/// The ID of the refusal of a connection because no slot is free.
pub(super) const ID_NO_FREE_INCOMING_CONNECTIONS: u8 = 0x14;
/// The ID of the notice that a peer disconnects.
pub(super) const ID_DISCONNECT_NOTIFICATION: u8 = 0x15;
/// The ID of the refusal of a connection over its protocol version.
pub(super) const ID_INCOMPATIBLE_PROTOCOL_VERSION: u8 = 0x19;
/// The ID of an unconnected pong.
pub(super) const ID_UNCONNECTED_PONG: u8 = 0x1c;
/// The first byte of every Minecraft game message.
pub(super) const ID_GAME: u8 = 0xfe;

/// The bytes of an IPv4 address after its family byte: four octets and the port.
const IPV4_ADDRESS_BYTES: usize = 6;
/// The bytes of an IPv6 address after its family byte (go-raknet `addrSize`).
const IPV6_ADDRESS_BYTES: usize = 28;
/// The bytes every offline message carries (RakNet `OFFLINE_MESSAGE_DATA_ID`).
pub(super) const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
/// The largest frame header: flags, length, message, sequence and order indices, channel, split.
pub(super) const MAX_FRAME_HEADER_BYTES: usize =
    Header::encoded_len(Reliability::ReliableSequenced, true);
/// The RakNet protocol version Minecraft speaks (go-raknet `protocolVersion`).
pub(super) const PROTOCOL_VERSION: u8 = 11;
/// Split count, split ID and split index of a fragment.
const SPLIT_HEADER_BYTES: usize = 10;
/// System addresses in `ConnectionRequestAccepted` (go-raknet `systemAddresses`).
const SYSTEM_ADDRESSES: usize = 20;
/// Serial number arithmetic: an index less than this far ahead of a base is ahead of it.
const U24_HALF: u32 = 0x0080_0000;
/// Sequence numbers and indices are 24-bit and wrap.
pub(super) const U24_MASK: u32 = 0x00ff_ffff;
/// IP and UDP header bytes that an MTU includes. RakNet and go-raknet count 28 for IPv6 too.
pub(super) const UDP_HEADER_BYTES: usize = 28;
/// The bytes of `UnconnectedPong` besides its data: ID, ping time, GUID, magic and data length.
pub(super) const UNCONNECTED_PONG_FIXED_BYTES: usize = 1 + 8 + 8 + MAGIC.len() + 2;
/// The address go-raknet writes for an unset system address (`putAddr` of a zero `AddrPort`).
const UNSET_ADDRESS: [u8; 7] = [4, 0xff, 0xff, 0xff, 0xff, 0, 0];

/// Reads big-endian numbers (and little-endian 24-bit ones) from a byte slice.
#[derive(Debug)]
pub(super) struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    pub(super) fn rest(&self) -> &'a [u8] {
        self.rest
    }

    /// Skips an address; its length follows go-raknet `addrSize` (family 0 or 4: IPv4).
    pub(super) fn address(&mut self) -> Result<()> {
        let len = match self.u8()? {
            0 | 4 => IPV4_ADDRESS_BYTES,
            _ => IPV6_ADDRESS_BYTES,
        };
        self.take(len).map(drop)
    }

    pub(super) fn magic(&mut self) -> Result<()> {
        if self.array()? == MAGIC {
            Ok(())
        } else {
            Err(Error::Malformed("offline message magic"))
        }
    }

    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (taken, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(Error::Malformed("truncated packet"))?;
        self.rest = rest;
        Ok(taken)
    }

    pub(super) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub(super) fn u24(&mut self) -> Result<u32> {
        let [a, b, c] = self.array()?;
        Ok(u32::from_le_bytes([a, b, c, 0]))
    }

    pub(super) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub(super) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub(super) fn u8(&mut self) -> Result<u8> {
        let [byte] = self.array()?;
        Ok(byte)
    }

    pub(super) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (taken, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(Error::Malformed("truncated packet"))?;
        self.rest = rest;
        Ok(*taken)
    }
}

/// How RakNet promises to deliver a frame. Wire values 5 to 7 (the "with ACK receipt" forms) are
/// read as 0, 2 and 3, as RakNet writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reliability {
    Unreliable,
    UnreliableSequenced,
    Reliable,
    ReliableOrdered,
    ReliableSequenced,
}

impl Reliability {
    /// Whether a frame carries an order index and channel.
    pub(super) const fn has_order(self) -> bool {
        self.is_sequenced() || matches!(self, Self::ReliableOrdered)
    }

    /// Whether a frame carries a message index and is resent until acknowledged.
    pub(super) const fn is_reliable(self) -> bool {
        matches!(
            self,
            Self::Reliable | Self::ReliableOrdered | Self::ReliableSequenced
        )
    }

    pub(super) const fn is_sequenced(self) -> bool {
        matches!(self, Self::UnreliableSequenced | Self::ReliableSequenced)
    }

    fn from_bits(bits: u8) -> Self {
        match bits {
            0 | 5 => Self::Unreliable,
            1 => Self::UnreliableSequenced,
            2 | 6 => Self::Reliable,
            3 | 7 => Self::ReliableOrdered,
            _ => Self::ReliableSequenced,
        }
    }

    fn bits(self) -> u8 {
        match self {
            Self::Unreliable => 0,
            Self::UnreliableSequenced => 1,
            Self::Reliable => 2,
            Self::ReliableOrdered => 3,
            Self::ReliableSequenced => 4,
        }
    }
}

/// The fragment fields of a split frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Split {
    pub(super) count: u32,
    pub(super) id: u16,
    pub(super) index: u32,
}

/// The header of one frame (go-raknet `packet`) inside a data datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Header {
    pub(super) reliability: Reliability,
    pub(super) message_index: u32,
    pub(super) sequence_index: u32,
    pub(super) order_index: u32,
    pub(super) channel: u8,
    pub(super) split: Option<Split>,
}

impl Header {
    /// The header bytes of a frame with `reliability`, split or not.
    pub(super) const fn encoded_len(reliability: Reliability, split: bool) -> usize {
        let mut len = 3;
        if reliability.is_reliable() {
            len += 3;
        }
        if reliability.is_sequenced() {
            len += 3;
        }
        if reliability.has_order() {
            len += 4;
        }
        if split {
            len += SPLIT_HEADER_BYTES;
        }
        len
    }

    /// One frame read from `cur`: its header and body. The length is in bits and rounds up, as in
    /// RakNet.
    pub(super) fn read<'a>(cur: &mut Cursor<'a>) -> Result<(Self, &'a [u8])> {
        let flags = cur.u8()?;
        let reliability = Reliability::from_bits(flags >> 5);
        let len = usize::from(cur.u16()?.div_ceil(8));
        if len == 0 {
            return Err(Error::Malformed("empty frame"));
        }
        let mut header = Self {
            reliability,
            message_index: 0,
            sequence_index: 0,
            order_index: 0,
            channel: 0,
            split: None,
        };
        if reliability.is_reliable() {
            header.message_index = cur.u24()?;
        }
        if reliability.is_sequenced() {
            header.sequence_index = cur.u24()?;
        }
        if reliability.has_order() {
            header.order_index = cur.u24()?;
            header.channel = cur.u8()?;
            if usize::from(header.channel) >= CHANNELS {
                return Err(Error::Malformed("ordering channel"));
            }
        }
        if flags & 0x10 != 0 {
            header.split = Some(Split {
                count: cur.u32()?,
                id: cur.u16()?,
                index: cur.u32()?,
            });
        }
        Ok((header, cur.take(len)?))
    }

    /// Writes the header of a frame whose body is `len` bytes.
    pub(super) fn write(&self, buf: &mut Vec<u8>, len: u16) -> Result<()> {
        let mut flags = self.reliability.bits() << 5;
        if self.split.is_some() {
            flags |= 0x10;
        }
        buf.push(flags);
        let bits = len
            .checked_mul(8)
            .ok_or(Error::Invariant("frame longer than 8191 bytes"))?;
        buf.extend_from_slice(&bits.to_be_bytes());
        if self.reliability.is_reliable() {
            put_u24(buf, self.message_index);
        }
        if self.reliability.is_sequenced() {
            put_u24(buf, self.sequence_index);
        }
        if self.reliability.has_order() {
            put_u24(buf, self.order_index);
            buf.push(self.channel);
        }
        if let Some(split) = self.split {
            buf.extend_from_slice(&split.count.to_be_bytes());
            buf.extend_from_slice(&split.id.to_be_bytes());
            buf.extend_from_slice(&split.index.to_be_bytes());
        }
        Ok(())
    }
}

/// Writes `UnconnectedPong` with the server list status `data`.
pub(super) fn unconnected_pong(
    buf: &mut Vec<u8>,
    ping_time: [u8; 8],
    guid: u64,
    data: &[u8],
) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| Error::Limit("pong data length"))?;
    buf.push(ID_UNCONNECTED_PONG);
    buf.extend_from_slice(&ping_time);
    buf.extend_from_slice(&guid.to_be_bytes());
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(data);
    Ok(())
}

/// Writes `OpenConnectionReply1`: with security on and `cookie`, or off without one.
pub(super) fn open_connection_reply_1(buf: &mut Vec<u8>, guid: u64, cookie: Option<u32>, mtu: u16) {
    buf.push(ID_OPEN_CONNECTION_REPLY_1);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&guid.to_be_bytes());
    // The security flag, then the cookie the client sends back in `OpenConnectionRequest2`
    // (go-raknet `OpenConnectionReply1`).
    match cookie {
        Some(cookie) => {
            buf.push(1);
            buf.extend_from_slice(&cookie.to_be_bytes());
        }
        None => buf.push(0),
    }
    buf.extend_from_slice(&mtu.to_be_bytes());
}

/// Writes `OpenConnectionReply2`.
pub(super) fn open_connection_reply_2(buf: &mut Vec<u8>, guid: u64, client: SocketAddr, mtu: u16) {
    buf.push(ID_OPEN_CONNECTION_REPLY_2);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&guid.to_be_bytes());
    put_address(buf, client);
    buf.extend_from_slice(&mtu.to_be_bytes());
    buf.push(0);
}

/// Writes `IncompatibleProtocolVersion`.
pub(super) fn incompatible_protocol_version(buf: &mut Vec<u8>, guid: u64) {
    buf.push(ID_INCOMPATIBLE_PROTOCOL_VERSION);
    buf.push(PROTOCOL_VERSION);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&guid.to_be_bytes());
}

/// Writes RakNet's reply to `OpenConnectionRequest2` when it has no free slot: ID, magic, GUID.
pub(super) fn no_free_incoming_connections(buf: &mut Vec<u8>, guid: u64) {
    buf.push(ID_NO_FREE_INCOMING_CONNECTIONS);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&guid.to_be_bytes());
}

/// The body (after the ID) of `ConnectionRequestAccepted`.
pub(super) fn connection_request_accepted(
    client: SocketAddr,
    request_time: u64,
    accepted_time: u64,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(
        1 + IPV6_ADDRESS_BYTES + 2 + SYSTEM_ADDRESSES * UNSET_ADDRESS.len() + 16,
    );
    put_address(&mut body, client);
    body.extend_from_slice(&0_u16.to_be_bytes());
    for _ in 0..SYSTEM_ADDRESSES {
        body.extend_from_slice(&UNSET_ADDRESS);
    }
    body.extend_from_slice(&request_time.to_be_bytes());
    body.extend_from_slice(&accepted_time.to_be_bytes());
    body
}

/// Writes an address as go-raknet `putAddr` does (IPv4-mapped IPv6 as IPv4).
pub(super) fn put_address(buf: &mut Vec<u8>, addr: SocketAddr) {
    match addr.ip().to_canonical() {
        IpAddr::V4(ip) => {
            buf.push(4);
            buf.extend(ip.octets().map(|octet| !octet));
        }
        IpAddr::V6(ip) => {
            buf.push(6);
            buf.extend_from_slice(&AF_INET6.to_le_bytes());
            buf.extend_from_slice(&addr.port().to_be_bytes());
            buf.extend_from_slice(&[0; 4]);
            buf.extend_from_slice(&ip.octets());
            buf.extend_from_slice(&[0; 4]);
            return;
        }
    }
    buf.extend_from_slice(&addr.port().to_be_bytes());
}

/// Writes an ACK or NAK (`flag`) with as many of the inclusive `ranges` as fit in `max_len`
/// bytes, and returns how many it wrote (at least one).
pub(super) fn put_ranges(
    buf: &mut Vec<u8>,
    flag: u8,
    ranges: &[(u32, u32)],
    max_len: usize,
) -> Result<usize> {
    buf.push(flag | FLAG_DATAGRAM);
    let count_at = buf.len();
    buf.extend_from_slice(&[0, 0]);
    let mut written: u16 = 0;
    for &(first, last) in ranges {
        let len = if first == last { 4 } else { 7 };
        if written > 0 && buf.len() + len > max_len {
            break;
        }
        if first == last {
            buf.push(1);
            put_u24(buf, first);
        } else {
            buf.push(0);
            put_u24(buf, first);
            put_u24(buf, last);
        }
        written = written.saturating_add(1);
    }
    let Some(count) = buf.get_mut(count_at..count_at + 2) else {
        return Err(Error::Invariant("acknowledgement count outside its buffer"));
    };
    count.copy_from_slice(&written.to_be_bytes());
    Ok(usize::from(written))
}

/// Reads the records of an ACK or NAK (after its flags byte) and passes each inclusive range on.
pub(super) fn read_ranges(
    data: &[u8],
    mut range: impl FnMut(u32, u32) -> Result<()>,
) -> Result<()> {
    let mut cur = Cursor::new(data);
    for _ in 0..cur.u16()? {
        match cur.u8()? {
            0 => {
                let first = cur.u24()?;
                range(first, cur.u24()?)?;
            }
            1 => {
                let index = cur.u24()?;
                range(index, index)?;
            }
            _ => return Err(Error::Malformed("acknowledgement record type")),
        }
    }
    Ok(())
}

pub(super) fn put_u24(buf: &mut Vec<u8>, value: u32) {
    let [a, b, c, _] = value.to_le_bytes();
    buf.extend_from_slice(&[a, b, c]);
}

/// How far `index` is ahead of `base` in 24-bit serial arithmetic, or `None` if it is behind.
pub(super) fn u24_ahead(index: u32, base: u32) -> Option<u32> {
    let distance = index.wrapping_sub(base) & U24_MASK;
    (distance < U24_HALF).then_some(distance)
}

/// The low 24 bits of `value`.
pub(super) fn low24(value: u64) -> u32 {
    let [a, b, c, ..] = value.to_le_bytes();
    u32::from_le_bytes([a, b, c, 0])
}
