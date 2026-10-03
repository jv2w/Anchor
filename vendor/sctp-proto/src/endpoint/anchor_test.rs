//! Reproduction test for the endpoint fix Anchor carries in this copy (Anchor PLAN.md 9.1). It
//! fails on the published 0.10.5 and passes here.

use alloc::boxed::Box;
use core::str::FromStr;

use super::*;
use crate::chunk::chunk_init::ChunkInit;
use crate::packet::{CommonHeader, Packet};

fn init(initiate_tag: u32) -> Bytes {
    Packet {
        common_header: CommonHeader {
            source_port: 5000,
            destination_port: 5000,
            verification_tag: 0,
        },
        chunks: vec![Box::new(ChunkInit {
            is_ack: false,
            initiate_tag,
            advertised_receiver_window_credit: 65_536,
            num_outbound_streams: 1,
            num_inbound_streams: 1,
            initial_tsn: 1,
            params: vec![],
        })],
    }
    .marshal()
    .unwrap()
}

/// RFC 4960 1.4: one association per pair of endpoints. 0.10.5 made a new association for each
/// INIT with a new Initiate Tag from the same remote; str0m drops those, and the endpoint kept
/// them all.
#[test]
fn a_second_init_from_a_remote_makes_no_second_association() {
    let mut endpoint = Endpoint::new(
        Arc::new(EndpointConfig::default()),
        Some(Arc::new(ServerConfig::default())),
    );
    let remote = SocketAddr::from_str("10.0.0.2:5000").unwrap();
    let now = Instant::now();
    let first = endpoint.handle(now, remote, None, None, init(1));
    assert!(matches!(first, Some((_, DatagramEvent::NewAssociation(_)))));
    for tag in 2..100 {
        assert!(endpoint.handle(now, remote, None, None, init(tag)).is_none());
    }
    assert_eq!(endpoint.associations.len(), 1);
    // Another remote still gets its own.
    let other = SocketAddr::from_str("10.0.0.3:5000").unwrap();
    let second = endpoint.handle(now, other, None, None, init(1_000));
    assert!(matches!(second, Some((_, DatagramEvent::NewAssociation(_)))));
}
