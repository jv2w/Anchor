//! Reproduction tests for the fixes Anchor carries in this copy (Anchor PLAN.md 9.1). Each one
//! fails on the published 0.10.5 and passes here.

use crate::chunk::chunk_selective_ack::ChunkSelectiveAck;
use crate::packet::CommonHeader;

use super::*;

const STREAM: StreamId = 1;
const WINDOW: u32 = 4096;

fn established() -> Association {
    let mut a = Association {
        state: AssociationState::Established,
        peer_last_tsn: 0,
        my_next_tsn: 1,
        max_receive_buffer_size: WINDOW,
        max_receive_message_size: WINDOW,
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

/// RFC 4960 6.5: DATA on a stream beyond the negotiated count is discarded; 0.10.5 created a
/// stream for any of the 65,536 identifiers.
#[test]
fn data_beyond_the_negotiated_streams_creates_no_stream() -> Result<()> {
    let mut a = established();
    a.my_max_num_inbound_streams = 2;
    let mut chunk = data(1, 0, false, 10);
    chunk.stream_identifier = 5;
    a.handle_data(&chunk)?;
    assert!(!a.streams.contains_key(&5));
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
