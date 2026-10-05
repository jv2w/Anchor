//! Reproduction tests for the fixes Anchor carries in this copy (Anchor PLAN.md 9.1). Each one
//! fails on the published 0.10.5 and passes here. Last, an ignored measurement of the receive
//! path (Anchor PLAN.md 10.5).

use crate::chunk::chunk_selective_ack::{ChunkSelectiveAck, GapAckBlock};
use crate::packet::CommonHeader;

use super::*;

const STREAM: StreamId = 1;
const WINDOW: u32 = 4096;

fn established() -> Association {
    established_with(WINDOW, WINDOW)
}

/// An established association with one stream, a `window` byte receive window and messages of
/// at most `message` bytes.
fn established_with(window: u32, message: u32) -> Association {
    let mut a = Association {
        state: AssociationState::Established,
        peer_last_tsn: 0,
        my_next_tsn: 1,
        max_receive_buffer_size: window,
        max_receive_message_size: message,
        max_payload_size: 1200,
        my_max_num_inbound_streams: 16,
        my_max_num_outbound_streams: 16,
        ..Default::default()
    };
    assert!(
        a.create_stream(STREAM, false, PayloadProtocolIdentifier::Binary)
            .is_some()
    );
    a
}

fn data(tsn: u32, ssn: u16, unordered: bool, len: usize) -> ChunkPayloadData {
    ChunkPayloadData {
        tsn,
        stream_identifier: STREAM,
        stream_sequence_number: ssn,
        unordered,
        beginning_fragment: true,
        ending_fragment: true,
        user_data: Bytes::from(vec![7; len]),
        ..Default::default()
    }
}

fn reassembly_bytes(a: &Association) -> usize {
    a.streams
        .values()
        .map(|s| s.get_num_bytes_in_reassembly_queue())
        .sum()
}

/// RFC 4960 6.2: past a full window only the chunk that advances the cumulative TSN is taken.
/// 0.10.5 took every chunk below the largest TSN received, so a gap let a peer queue without
/// limit.
#[test]
fn data_past_a_full_window_is_dropped_unless_it_advances_the_cumulative_tsn() -> Result<()> {
    let mut a = established();
    // The largest TSN, far ahead: TSN 1 stays missing, so nothing is delivered.
    a.handle_data(&data(100_000, 50_000, false, 100))?;
    for tsn in 2..2_000 {
        a.handle_data(&data(tsn, u16::try_from(tsn).unwrap(), false, 100))?;
    }
    assert!(
        reassembly_bytes(&a) <= WINDOW as usize + 100,
        "{} bytes held behind a {WINDOW} byte window",
        reassembly_bytes(&a)
    );
    // The missing TSN still gets in and lets the queue drain.
    a.handle_data(&data(1, 0, false, 100))?;
    assert!(a.peer_last_tsn > 1);
    Ok(())
}

/// Read data must not stay alive behind a TSN gap: 0.10.5 kept every received chunk's bytes in
/// the receive payload queue until the gap closed, outside the receive window.
#[test]
fn read_data_is_not_held_behind_a_tsn_gap() -> Result<()> {
    let mut a = established();
    for tsn in 2..2_000 {
        a.handle_data(&data(tsn, 0, true, 100))?;
        while a.stream(STREAM)?.read()?.is_some() {}
    }
    assert_eq!(a.payload_queue.get_num_bytes(), 0);
    let tracked = a.payload_queue.len();
    let record = core::mem::size_of::<ChunkPayloadData>();
    assert!(
        tracked <= WINDOW as usize / record + 1,
        "{tracked} TSNs held behind a {WINDOW} byte window"
    );
    Ok(())
}

/// RFC 4960 6.2: DATA without user data ends the association; 0.10.5 queued it uncounted.
#[test]
fn data_without_user_data_is_refused() {
    let mut a = established();
    let result = a.handle_data(&data(1, 0, false, 0));
    assert!(result.is_err(), "{result:?}");
    assert!(a.payload_queue.is_empty());
}

/// RFC 4960 5.1.1: we receive on at most the peer's outbound streams. 0.10.5 used the peer's
/// inbound count instead.
#[test]
fn the_inbound_stream_count_follows_the_peer_outbound_streams() -> Result<()> {
    let mut a = Association {
        my_max_num_inbound_streams: 100,
        my_max_num_outbound_streams: 100,
        ..Default::default()
    };
    let init = ChunkInit {
        is_ack: false,
        initiate_tag: 1,
        advertised_receiver_window_credit: 65_536,
        num_outbound_streams: 2,
        num_inbound_streams: 10,
        initial_tsn: 1,
        params: vec![],
    };
    let packet = Packet {
        common_header: CommonHeader {
            source_port: 5000,
            destination_port: 5000,
            verification_tag: 0,
        },
        chunks: vec![],
    };
    a.handle_init(&packet, &init)?;
    assert_eq!(a.my_max_num_inbound_streams, 2);
    assert_eq!(a.my_max_num_outbound_streams, 10);
    Ok(())
}

/// RFC 4960 6.5: DATA on a stream beyond the negotiated count is acknowledged and discarded;
/// 0.10.5 created a stream for any of the 65,536 identifiers. Its TSN still counts, or the
/// cumulative TSN would stall behind it.
#[test]
fn data_beyond_the_negotiated_streams_creates_no_stream() -> Result<()> {
    let mut a = established();
    a.my_max_num_inbound_streams = 2;
    let mut chunk = data(1, 0, false, 10);
    chunk.stream_identifier = 5;
    a.handle_data(&chunk)?;
    assert!(!a.streams.contains_key(&5));
    a.handle_data(&data(2, 0, false, 10))?;
    assert_eq!(a.peer_last_tsn, 2);
    assert!(a.payload_queue.is_empty());
    assert!(a.stream(STREAM)?.read()?.is_some());
    Ok(())
}

/// A stream opened, closed and opened again is queued for acceptance once: 0.10.5 queued it on
/// every reopening, and str0m never accepts, so the queue only grew.
#[test]
fn a_reopened_stream_is_queued_once() -> Result<()> {
    let mut a = established();
    for round in 0..10_u32 {
        let mut chunk = data(round + 1, 0, false, 10);
        chunk.stream_identifier = 3;
        a.handle_data(&chunk)?;
        while a.stream(3)?.read()?.is_some() {}
        a.unregister_stream(3, false);
    }
    assert_eq!(a.stream_queue.iter().filter(|&&id| id == 3).count(), 1);
    Ok(())
}

/// TSNs are serial numbers (RFC 1982): acknowledging across the u32 wrap must not overflow.
/// 0.10.5 added 1 with `+`, which panics in a debug build there.
#[test]
fn a_sack_across_the_tsn_wrap_is_processed() -> Result<()> {
    let mut a = established();
    a.cumulative_tsn_ack_point = u32::MAX - 1;
    a.advanced_peer_tsn_ack_point = u32::MAX - 1;
    a.my_next_tsn = 1;
    for tsn in [u32::MAX, 0] {
        a.inflight_queue.push_no_check(ChunkPayloadData {
            tsn,
            nsent: 1,
            ..data(tsn, 0, false, 10)
        });
    }
    let sack = ChunkSelectiveAck {
        cumulative_tsn_ack: 0,
        advertised_receiver_window_credit: 65_536,
        gap_ack_blocks: vec![],
        duplicate_tsn: vec![],
    };
    a.handle_sack(&sack, Instant::now())?;
    assert_eq!(a.cumulative_tsn_ack_point, 0);
    assert!(a.inflight_queue.is_empty());
    Ok(())
}

/// A gap block past the TSN wrap: 0.10.5 added its offset with `+`, which panics in a debug
/// build when the cumulative TSN is near `u32::MAX`.
#[test]
fn a_sack_gap_block_across_the_tsn_wrap_is_processed() -> Result<()> {
    let mut a = established();
    a.cumulative_tsn_ack_point = u32::MAX - 1;
    a.advanced_peer_tsn_ack_point = u32::MAX - 1;
    a.my_next_tsn = 2;
    for tsn in [u32::MAX, 0, 1] {
        a.inflight_queue.push_no_check(ChunkPayloadData {
            tsn,
            nsent: 1,
            ..data(tsn, 0, false, 10)
        });
    }
    let sack = ChunkSelectiveAck {
        cumulative_tsn_ack: u32::MAX - 1,
        advertised_receiver_window_credit: 65_536,
        gap_ack_blocks: vec![GapAckBlock { start: 2, end: 3 }],
        duplicate_tsn: vec![],
    };
    a.handle_sack(&sack, Instant::now())?;
    assert!(a.inflight_queue.get(0).is_some_and(|c| c.acked));
    assert!(a.inflight_queue.get(1).is_some_and(|c| c.acked));
    assert!(a.inflight_queue.get(u32::MAX).is_some_and(|c| !c.acked));
    Ok(())
}

/// Gap offsets are 16 bits; 0.10.5 truncated larger ones and overflowed `end + 1` (a debug
/// panic a peer triggers with two TSNs 65,535 and 65,536 past the cumulative TSN).
#[test]
fn gap_reports_stop_at_the_16_bit_offset_limit() {
    let mut a = established();
    for offset in [65_535_u32, 65_536] {
        a.payload_queue.push(data(offset, 0, true, 1), 0);
    }
    let blocks = a.payload_queue.get_gap_ack_blocks(0);
    assert_eq!(blocks.len(), 1);
    assert_eq!((blocks[0].start, blocks[0].end), (65_535, 65_535));
}

/// Every pushed TSN sorted the whole key list again: O(n) per chunk sent (the in-flight queue)
/// or received out of order. 60,000 in-order pushes took seconds in 0.10.5 and take
/// milliseconds with a sorted insert; the bound leaves a wide margin both ways.
#[test]
fn pushing_a_tsn_does_not_sort_the_whole_queue() {
    const TSNS: u32 = 60_000;
    let mut queue = PayloadQueue::new();
    let start = Instant::now();
    // Across the TSN wrap, so serial-number order is what keeps the keys sorted.
    for tsn in (u32::MAX - TSNS / 2)..=u32::MAX {
        queue.push_no_check(data(tsn, 0, false, 1));
    }
    for tsn in 0..TSNS / 2 {
        queue.push_no_check(data(tsn, 0, false, 1));
    }
    let took = start.elapsed();
    assert!(took < Duration::from_secs(1), "{TSNS} pushes took {took:?}");
    let mut expected = u32::MAX - TSNS / 2;
    while let Some(chunk) = queue.pop(expected) {
        assert_eq!(chunk.tsn, expected);
        expected = expected.wrapping_add(1);
    }
    assert_eq!(expected, TSNS / 2, "every TSN popped in order");
}

/// Every outbound poll walked all chunks in flight looking for ones a T3-rtx timeout marked,
/// though only that timeout marks them: O(in flight) per poll, the largest share of a sending
/// peer's cost (Anchor PLAN.md 10.5). 20,000 polls over 2,000 chunks took seconds in 0.10.5.
/// Marked chunks still go out, once.
#[test]
fn polling_without_a_retransmission_timeout_skips_the_chunks_in_flight() {
    const IN_FLIGHT: u32 = 2_000;
    const POLLS: u32 = 20_000;
    let mut a = established();
    a.cwnd = u32::MAX;
    a.rwnd = u32::MAX;
    a.cumulative_tsn_ack_point = 0;
    for tsn in 1..=IN_FLIGHT {
        a.inflight_queue.push_no_check(data(tsn, 0, false, 10));
    }
    let now = Instant::now();
    for _ in 0..POLLS {
        assert!(a.get_data_packets_to_retransmit(now).is_empty());
    }
    let took = now.elapsed();
    assert!(took < Duration::from_millis(500), "{POLLS} polls took {took:?}");

    a.inflight_queue.mark_all_to_retrasmit();
    let resent: usize = a
        .get_data_packets_to_retransmit(now)
        .iter()
        .map(|packet| packet.chunks.len())
        .sum();
    assert_eq!(resent, IN_FLIGHT as usize);
    assert!(a.get_data_packets_to_retransmit(now).is_empty());
}

/// Receive cost of Anchor's largest NetherNet message: 256 data channel messages of 262,144
/// bytes (a header byte and a 262,143-byte segment), cut into DATA chunks of the 1,062 bytes
/// str0m sends at its 1,150-byte MTU, into a 1 MiB window (str0m's default), read as they
/// complete. Three arrival orders: in order; each window-sized batch reversed; and 10% loss,
/// each lost chunk (again with 10% loss) arriving 100 chunks later, as a retransmission an
/// RTT later would. Run in release:
/// `cargo test --release --manifest-path vendor/sctp-proto/Cargo.toml --lib receive_cost -- --ignored --nocapture`
#[test]
#[ignore = "measurement; run in release with --ignored --nocapture"]
fn receive_cost_of_the_largest_message() -> Result<()> {
    const MESSAGES: usize = 256;
    const MESSAGE: usize = 262_144;
    const CHUNK: usize = 1_062;
    const RECEIVE_WINDOW: u32 = 1024 * 1024;
    const ROUNDS: usize = 5;

    let payload = Bytes::from(vec![7; CHUNK]);
    let mut chunks = Vec::new();
    let mut tsn = 1u32;
    for ssn in 0..MESSAGES {
        let mut offset = 0;
        while offset < MESSAGE {
            let len = CHUNK.min(MESSAGE - offset);
            chunks.push(ChunkPayloadData {
                tsn,
                stream_identifier: STREAM,
                stream_sequence_number: u16::try_from(ssn).unwrap(),
                beginning_fragment: offset == 0,
                ending_fragment: offset + len == MESSAGE,
                user_data: payload.slice(..len),
                ..Default::default()
            });
            tsn += 1;
            offset += len;
        }
    }
    // Whole messages that fit the window, each chunk held behind the gap costing its data and a
    // TSN record: about the most a sender can have in flight.
    let per_message = MESSAGE.div_ceil(CHUNK);
    let batch =
        RECEIVE_WINDOW as usize / (CHUNK + RECEIVED_TSN_COST as usize) / per_message * per_message;
    let in_order: Vec<usize> = (0..chunks.len()).collect();
    let reversed: Vec<usize> = in_order
        .chunks(batch)
        .flat_map(|batch| batch.iter().rev().copied())
        .collect();
    let mut lossy = Vec::with_capacity(chunks.len() * 12 / 10);
    let mut late: alloc::collections::VecDeque<(usize, usize)> = Default::default();
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let lost = |rng: &mut u64| {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        *rng % 10 == 0
    };
    let mut slot = 0;
    for i in 0..chunks.len() {
        while late.front().is_some_and(|&(due, _)| due <= slot) {
            let (_, j) = late.pop_front().unwrap();
            if lost(&mut rng) {
                late.push_back((slot + 100, j));
            } else {
                lossy.push(j);
            }
            slot += 1;
        }
        if lost(&mut rng) {
            late.push_back((slot + 100, i));
        } else {
            lossy.push(i);
        }
        slot += 1;
    }
    while let Some((_, j)) = late.pop_front() {
        lossy.push(j);
    }

    for (name, order) in [
        ("in order", &in_order),
        ("window batches reversed", &reversed),
        ("10% loss", &lossy),
    ] {
        let mut best = std::time::Duration::MAX;
        for _ in 0..ROUNDS {
            let mut a = established_with(RECEIVE_WINDOW, MESSAGE as u32);
            let mut read = 0;
            let start = Instant::now();
            for &i in order.iter() {
                a.handle_data(&chunks[i])?;
                while let Some(message) = a.stream(STREAM)?.read()? {
                    assert_eq!(message.len(), MESSAGE);
                    read += 1;
                }
            }
            best = best.min(start.elapsed());
            assert_eq!(read, MESSAGES, "{name}: a chunk was dropped");
        }
        std::println!(
            "{name}: {} chunks, {:?} for {MESSAGES} messages, {:?} per message, {:.0} ns per chunk",
            order.len(),
            best,
            best / MESSAGES as u32,
            best.as_nanos() as f64 / order.len() as f64
        );
    }
    Ok(())
}
