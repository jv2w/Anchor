//! NetherNet message segments on `ReliableDataChannel`: each data channel message is one header
//! byte, the number of segments that follow it, and a payload (go-nethernet `message.go`).

use std::fmt;
use std::slice::Chunks;

/// The most segments in one message: the header byte counts the segments after the first, so it
/// holds at most `u8::MAX` of them.
const MAX_SEGMENTS: usize = 256;

/// A segment framing failure. Each one closes the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    /// A data channel message without the header byte.
    Empty,
    /// A segment with a header byte and no payload (go-nethernet requires at least 2 bytes).
    EmptySegment,
    /// A continuation segment whose header is not one less than the previous one.
    Sequence {
        /// The header the continuation must carry.
        expected: u8,
        /// The header it carried.
        got: u8,
    },
    /// The message grows past the reassembly limit.
    TooLarge,
    /// A message to send needs more than [`MAX_SEGMENTS`] segments.
    TooManySegments,
    /// A segment payload limit of zero.
    ZeroPayload,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty data channel message"),
            Self::EmptySegment => f.write_str("segment without payload"),
            Self::Sequence { expected, got } => {
                write!(f, "segment count {got} where {expected} was due")
            }
            Self::TooLarge => f.write_str("message exceeds the reassembly limit"),
            Self::TooManySegments => {
                write!(f, "message needs more than {MAX_SEGMENTS} segments")
            }
            Self::ZeroPayload => f.write_str("segment payload limit is zero"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;

/// Joins the segments of one reliable channel into whole messages.
///
/// Memory grows with the bytes received, never with the count a header promises, so a header of
/// 255 costs nothing until its payloads arrive. After an error the peer closes; the state is not
/// reused.
#[derive(Debug)]
pub(super) struct Reassembler {
    max_message_bytes: usize,
    /// Payloads of the message in progress.
    buffer: Vec<u8>,
    /// The header of the last segment while a message is in progress.
    remaining: Option<u8>,
}

impl Reassembler {
    /// A reassembler for messages of at most `max_message_bytes` bytes.
    pub(super) fn new(max_message_bytes: usize) -> Self {
        Self {
            max_message_bytes,
            buffer: Vec::new(),
            remaining: None,
        }
    }

    /// The bytes of the message in progress.
    #[cfg(test)]
    pub(super) fn held_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Takes one data channel message; returns the whole message when this segment ends it. The
    /// first segment's allocation becomes the message, with its header byte shifted out, so a
    /// one-segment message gets no allocation of its own.
    pub(super) fn push(&mut self, mut segment: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let (&count, payload) = segment.split_first().ok_or(Error::Empty)?;
        if payload.is_empty() {
            return Err(Error::EmptySegment);
        }
        if let Some(previous) = self.remaining {
            let expected = previous.saturating_sub(1);
            if count != expected {
                return Err(Error::Sequence {
                    expected,
                    got: count,
                });
            }
        }
        let total = self
            .buffer
            .len()
            .checked_add(payload.len())
            .ok_or(Error::TooLarge)?;
        if total > self.max_message_bytes {
            return Err(Error::TooLarge);
        }
        if self.remaining.is_none() {
            segment.remove(0);
            if count == 0 {
                return Ok(Some(segment));
            }
            self.buffer = segment;
            self.remaining = Some(count);
            return Ok(None);
        }
        self.buffer.extend_from_slice(payload);
        if count == 0 {
            self.remaining = None;
            return Ok(Some(std::mem::take(&mut self.buffer)));
        }
        self.remaining = Some(count);
        Ok(None)
    }

    /// Sets the largest message the next segment may complete; the bytes already held stay.
    pub(super) fn set_max_message_bytes(&mut self, max_message_bytes: usize) {
        self.max_message_bytes = max_message_bytes;
    }
}

/// The segments of one message, first to last; made by [`segments`].
#[derive(Debug)]
pub(super) struct Segments<'a> {
    chunks: Chunks<'a, u8>,
    next_header: u8,
}

impl Segments<'_> {
    /// The header of the next segment.
    pub(super) fn header(&self) -> u8 {
        self.next_header
    }
}

impl<'a> Iterator for Segments<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let payload = self.chunks.next()?;
        let header = self.next_header;
        self.next_header = header.saturating_sub(1);
        Some((header, payload))
    }
}

/// Splits `message` into segments of at most `max_payload` bytes each. Yields `(header,
/// payload)`: the header counts the segments after this one.
///
/// An empty message yields no segments, as go-nethernet sends nothing for empty data; Anchor never
/// sends an empty batch.
pub(super) fn segments(message: &[u8], max_payload: usize) -> Result<Segments<'_>> {
    if max_payload == 0 {
        return Err(Error::ZeroPayload);
    }
    let count = message.len().div_ceil(max_payload);
    let next_header = u8::try_from(count.saturating_sub(1)).map_err(|_| Error::TooManySegments)?;
    Ok(Segments {
        chunks: message.chunks(max_payload),
        next_header,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// go-nethernet's segment payload (`maxMessageSize`): 262,144 minus the header byte.
    const MAX_PAYLOAD_BYTES: usize = 262_143;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251).to_le_bytes()[0]).collect()
    }

    fn framed(message: &[u8], max_payload: usize) -> Vec<Vec<u8>> {
        segments(message, max_payload)
            .unwrap()
            .map(|(header, payload)| {
                let mut segment = vec![header];
                segment.extend_from_slice(payload);
                segment
            })
            .collect()
    }

    fn round_trip(len: usize, max_payload: usize, max_message_bytes: usize) -> Vec<u8> {
        let message = pattern(len);
        let mut reassembler = Reassembler::new(max_message_bytes);
        let framed = framed(&message, max_payload);
        let (last, rest) = framed.split_last().unwrap();
        for segment in rest {
            assert_eq!(reassembler.push(segment.clone()), Ok(None));
        }
        let whole = reassembler.push(last.clone()).unwrap().unwrap();
        assert_eq!(whole, message);
        whole
    }

    #[test]
    fn empty_message_has_no_segments() {
        assert_eq!(segments(&[], MAX_PAYLOAD_BYTES).unwrap().count(), 0);
    }

    #[test]
    fn one_byte_message_is_one_segment() {
        assert_eq!(framed(&[7], MAX_PAYLOAD_BYTES), vec![vec![0, 7]]);
        round_trip(1, MAX_PAYLOAD_BYTES, 1);
    }

    #[test]
    fn full_segment_fits_one_segment() {
        let framed = framed(&pattern(MAX_PAYLOAD_BYTES), MAX_PAYLOAD_BYTES);
        assert_eq!(framed.len(), 1);
        assert_eq!(framed[0].len(), 262_144);
        round_trip(MAX_PAYLOAD_BYTES, MAX_PAYLOAD_BYTES, MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn one_byte_over_a_segment_splits_in_two() {
        let framed = framed(&pattern(262_144), MAX_PAYLOAD_BYTES);
        assert_eq!(framed.len(), 2);
        assert_eq!((framed[0][0], framed[0].len()), (1, 262_144));
        assert_eq!((framed[1][0], framed[1].len()), (0, 2));
        round_trip(262_144, MAX_PAYLOAD_BYTES, 262_144);
    }

    #[test]
    fn most_segments_round_trip_and_one_more_is_refused() {
        let segment = 1000;
        let framed = framed(&pattern(MAX_SEGMENTS * segment), segment);
        assert_eq!(framed.len(), 256);
        assert_eq!(framed[0][0], 255);
        round_trip(MAX_SEGMENTS * segment, segment, MAX_SEGMENTS * segment);
        assert_eq!(
            segments(&pattern(MAX_SEGMENTS * segment + 1), segment).err(),
            Some(Error::TooManySegments)
        );
    }

    #[test]
    fn zero_payload_limit_is_refused() {
        assert_eq!(segments(&[1], 0).err(), Some(Error::ZeroPayload));
    }

    #[test]
    fn message_at_the_limit_passes_and_one_byte_over_fails() {
        round_trip(10_000, 3000, 10_000);
        let mut reassembler = Reassembler::new(9999);
        let framed = framed(&pattern(10_000), 3000);
        for segment in &framed[..3] {
            assert_eq!(reassembler.push(segment.clone()), Ok(None));
        }
        assert_eq!(reassembler.push(framed[3].clone()), Err(Error::TooLarge));
    }

    #[test]
    fn single_segment_over_the_limit_fails() {
        let mut reassembler = Reassembler::new(4);
        assert_eq!(
            reassembler.push(vec![0, 1, 2, 3, 4, 5]),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn header_count_does_not_reserve_memory() {
        let mut reassembler = Reassembler::new(1 << 20);
        assert_eq!(reassembler.push(vec![255, 1]), Ok(None));
        assert!(reassembler.buffer.capacity() < 1024);
    }

    #[test]
    fn a_limit_set_later_applies_to_the_next_segment() {
        let mut reassembler = Reassembler::new(2);
        assert_eq!(reassembler.push(vec![0, 1, 2, 3]), Err(Error::TooLarge));
        reassembler.set_max_message_bytes(3);
        assert_eq!(reassembler.push(vec![0, 1, 2, 3]), Ok(Some(vec![1, 2, 3])));
    }

    #[test]
    fn empty_data_channel_message_fails() {
        assert_eq!(Reassembler::new(16).push(Vec::new()), Err(Error::Empty));
    }

    #[test]
    fn header_without_payload_fails() {
        assert_eq!(Reassembler::new(16).push(vec![0]), Err(Error::EmptySegment));
        let mut reassembler = Reassembler::new(16);
        assert_eq!(reassembler.push(vec![1, 9]), Ok(None));
        assert_eq!(reassembler.push(vec![0]), Err(Error::EmptySegment));
    }

    #[test]
    fn continuation_with_wrong_count_fails() {
        let mut reassembler = Reassembler::new(16);
        assert_eq!(reassembler.push(vec![2, 1]), Ok(None));
        assert_eq!(
            reassembler.push(vec![2, 1]),
            Err(Error::Sequence {
                expected: 1,
                got: 2
            })
        );
        let mut reassembler = Reassembler::new(16);
        assert_eq!(reassembler.push(vec![2, 1]), Ok(None));
        assert_eq!(
            reassembler.push(vec![0, 1]),
            Err(Error::Sequence {
                expected: 1,
                got: 0
            })
        );
    }

    #[test]
    fn messages_follow_each_other() {
        let mut reassembler = Reassembler::new(16);
        assert_eq!(reassembler.push(vec![1, 1, 2]), Ok(None));
        assert_eq!(reassembler.push(vec![0, 3]), Ok(Some(vec![1, 2, 3])));
        assert_eq!(reassembler.push(vec![0, 4]), Ok(Some(vec![4])));
        assert_eq!(reassembler.push(vec![3, 5]), Ok(None));
    }
}
