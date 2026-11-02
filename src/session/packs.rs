//! The resource pack sequence of a session, and the chunks of the packs it downloads.
//!
//! What a client may ask for between `ResourcePacksInfo` and its `Completed` answer is checked.
//! The client asks for the packs it lacks once,
//! before the stack, and for each chunk of them once.
//!
//! The chunks go out in request order at most one per [`CHUNK_INTERVAL`] (gophertunnel), and
//! only while the connection holds at most [`MAX_UNDELIVERED_BYTES`] bytes it has not delivered
//! (each chunk's acknowledgement is waited for): a slow link slows the download instead of
//! filling the transport's outbound cap. Asking for packs extends the spawn deadline by
//! [`DOWNLOAD_BASE`] and each chunk sent by [`CHUNK_ALLOWANCE`], so the transfer does not eat into
//! the 60 seconds a client has to spawn, but a client that asks and never downloads gains only the
//! base.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use crate::pack::{Offer, Pack};
use crate::protocol::Bitset;
use crate::protocol::consts::PackResponse;
use crate::protocol::packet::{
    Packet, ResourcePackChunkData, ResourcePackChunkRequest, ResourcePackClientResponse,
    ResourcePackDataInfo,
};

/// What each chunk sent adds to the spawn deadline: ten times [`CHUNK_INTERVAL`], so a client may
/// download at 64 KiB/s.
pub(super) const CHUNK_ALLOWANCE: Duration = Duration::from_secs(2);
/// Bytes per chunk.
/// This is gophertunnel's `DefaultResourcePackChunkSize`; a chunk of 256 KiB with its
/// packet header no longer fits one 256 KiB batch.
pub(super) const CHUNK_BYTES: u32 = 128 * 1024;
/// The least time between two chunks to one client.
/// This is gophertunnel's `DefaultResourcePackChunkSendDelay`: clients after 1.26.30 may fail
/// downloads sent faster.
pub(super) const CHUNK_INTERVAL: Duration = Duration::from_millis(200);
/// What asking for packs adds to the spawn deadline.
pub(super) const DOWNLOAD_BASE: Duration = Duration::from_secs(30);
/// The most bytes the connection may still have to deliver when the next chunk goes out.
/// That is about one chunk, so a download keeps at most about two chunks in the transport however
/// slow the link is.
pub(super) const MAX_UNDELIVERED_BYTES: usize = CHUNK_BYTES as usize;

/// One session's progress through the pack sequence.
#[derive(Debug, Default)]
pub(super) struct Transfer {
    /// The client asked which packs to download (`SendPacks`).
    requested: bool,
    /// The client has every pack and got the stack.
    stacked: bool,
    /// Per offered pack, once the client asked for it, the chunks it has asked for.
    chunks: Vec<Option<Bitset>>,
    /// Chunk requests waiting for their send time, in arrival order.
    /// Each chunk is asked for once, so the queue holds at most the chunks of the offer.
    queue: VecDeque<(usize, u32)>,
}

impl Transfer {
    /// Whether chunks wait to be sent.
    pub(super) fn has_chunks(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Takes the next waiting chunk.
    pub(super) fn next_chunk(&mut self, offer: &Offer) -> Option<Packet> {
        let (index, chunk) = self.queue.pop_front()?;
        let start = chunk as usize * CHUNK_BYTES as usize;
        #[expect(
            clippy::expect_used,
            reason = "invariant: `on_chunk_request` queues only chunks inside an offered pack"
        )]
        let data = offer
            .packs()
            .get(index)
            .map(Pack::bytes)
            .and_then(|bytes| {
                bytes.get(start..start.saturating_add(CHUNK_BYTES as usize).min(bytes.len()))
            })
            .expect("a queued chunk lies inside its pack");
        Some(Packet::ResourcePackChunkData(ResourcePackChunkData {
            uuid: offer.id(index).to_owned(),
            chunk_index: chunk,
            data_offset: start as u64,
            data: data.to_vec(),
        }))
    }

    /// Handles a `ResourcePackChunkRequest`.
    /// It queues the chunk, or fails if its pack was not requested, the chunk does not exist or
    /// was asked for before.
    pub(super) fn on_chunk_request(
        &mut self,
        offer: &Offer,
        request: &ResourcePackChunkRequest,
    ) -> Result<()> {
        let index = offer
            .find(&request.uuid)
            .ok_or(Error::Order("chunk of a pack that was not offered"))?;
        let sent = self
            .chunks
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Error::Order("chunk of a pack that was not requested"))?;
        let chunk = u32::try_from(request.chunk_index)
            .ok()
            .filter(|&chunk| (chunk as usize) < sent.len())
            .ok_or(Error::Order("chunk index out of range"))?;
        let bit = chunk as usize;
        if sent.get(bit) {
            return Err(Error::Order("chunk requested twice"));
        }
        sent.set(bit);
        self.queue.push_back((index, chunk));
        Ok(())
    }

    /// Handles a `ResourcePackClientResponse`.
    pub(super) fn on_response(
        &mut self,
        offer: &Offer,
        response: &ResourcePackClientResponse,
    ) -> Result<Reply> {
        match PackResponse::try_from(response.response) {
            Ok(PackResponse::Refused) => Ok(Reply::Refused),
            Ok(PackResponse::SendPacks) => self.on_send_packs(offer, &response.packs_to_download),
            Ok(PackResponse::AllPacksDownloaded) => {
                if self.stacked {
                    return Err(Error::Order("stack requested twice"));
                }
                self.stacked = true;
                Ok(Reply::Send {
                    packets: vec![Packet::ResourcePackStack(offer.stack().clone())],
                    extend: Duration::ZERO,
                })
            }
            Ok(PackResponse::Completed) => {
                // Chunks still queued are of no use once the client goes on.
                self.queue = VecDeque::new();
                Ok(Reply::Join)
            }
            Err(_) => Err(Error::Order("unknown pack response")),
        }
    }

    /// `SendPacks`: the data info of each pack the client names.
    fn on_send_packs(&mut self, offer: &Offer, names: &[String]) -> Result<Reply> {
        if self.requested {
            return Err(Error::Order("packs requested twice"));
        }
        if self.stacked {
            return Err(Error::Order("packs requested after the stack"));
        }
        self.requested = true;
        let packs = offer.packs();
        if names.len() > packs.len() {
            return Err(Error::Limit("more packs requested than offered"));
        }
        let mut chunks = vec![None; packs.len()];
        let mut packets = Vec::with_capacity(names.len());
        for name in names {
            let index = offer
                .find(name)
                .ok_or(Error::Order("requested a pack that was not offered"))?;
            #[expect(
                clippy::expect_used,
                reason = "invariant: a found index has a pack and a slot"
            )]
            let (slot, pack) = chunks
                .get_mut(index)
                .zip(packs.get(index))
                .expect("a found pack has a slot");
            if slot.is_some() {
                return Err(Error::Order("pack requested twice"));
            }
            let count = chunk_count(pack.size_bytes());
            *slot = Some(Bitset::new(count as usize));
            packets.push(Packet::ResourcePackDataInfo(ResourcePackDataInfo {
                uuid: offer.id(index).to_owned(),
                data_chunk_size: CHUNK_BYTES,
                chunk_count: count,
                size: pack.size_bytes(),
                hash: pack.sha256().to_vec(),
                premium: false,
                pack_type: pack.pack_type(),
            }));
        }
        self.chunks = chunks;
        let extend = if names.is_empty() {
            Duration::ZERO
        } else {
            DOWNLOAD_BASE
        };
        Ok(Reply::Send { packets, extend })
    }
}

/// What the session does after a pack response.
#[derive(Debug)]
pub(super) enum Reply {
    /// Send `packets` and move the spawn deadline `extend` later.
    Send {
        packets: Vec<Packet>,
        extend: Duration,
    },
    /// The client has applied the stack: the player joins.
    Join,
    /// The client refused the packs: it is told why and disconnected.
    Refused,
}

/// A client that broke the pack sequence; the session closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    /// A response or request out of order, repeated or naming what was not offered.
    Order(&'static str),
    /// More than the offer holds.
    Limit(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Order(what) => write!(f, "resource pack sequence broken: {what}"),
            Self::Limit(what) => write!(f, "resource pack limit exceeded: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub(super) type Result<T, E = Error> = std::result::Result<T, E>;

/// The chunks of a pack of `size` bytes.
/// Packs are at most 256 MiB, so the count fits `u32` easily.
fn chunk_count(size: u64) -> u32 {
    #[expect(
        clippy::expect_used,
        reason = "invariant: a pack is at most `MAX_PACK_BYTES`, which is a few thousand chunks"
    )]
    u32::try_from(size.div_ceil(u64::from(CHUNK_BYTES))).expect("a pack has few chunks")
}

/// How much later sending `bytes` before the spawn moves the spawn deadline.
/// That is the time they take at the download's floor rate, [`CHUNK_ALLOWANCE`] per
/// [`CHUNK_BYTES`].
pub(super) fn send_allowance(bytes: usize) -> Duration {
    let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
    CHUNK_ALLOWANCE.saturating_mul(bytes) / CHUNK_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pack::Acceptance;
    use crate::pack::tests::{UUID, pack};
    use crate::protocol::consts::ResourcePackType;

    const OTHER: &str = "11111111-2222-3333-4444-555555555555";

    fn offer() -> Offer {
        // 300 KiB: three chunks, the last one short.
        let big = pack(UUID, "1.0.0", 300 * 1024);
        Offer::new(vec![big, pack(OTHER, "2.0.0", 10)], Acceptance::Required).unwrap()
    }

    fn response(response: PackResponse, packs: &[String]) -> ResourcePackClientResponse {
        ResourcePackClientResponse {
            response: u32::from(response),
            packs_to_download: packs.to_vec(),
        }
    }

    fn chunk(uuid: &str, chunk_index: i32) -> ResourcePackChunkRequest {
        ResourcePackChunkRequest {
            uuid: uuid.to_owned(),
            chunk_index,
        }
    }

    fn sent(reply: Reply) -> (Vec<Packet>, Duration) {
        match reply {
            Reply::Send { packets, extend } => (packets, extend),
            other => panic!("expected packets, got {other:?}"),
        }
    }

    #[test]
    fn a_download_sends_data_info_then_every_chunk_once() {
        let offer = offer();
        let mut t = Transfer::default();
        let id = offer.id(0).to_owned();
        let (packets, extend) = sent(
            t.on_response(
                &offer,
                &response(PackResponse::SendPacks, std::slice::from_ref(&id)),
            )
            .unwrap(),
        );
        let [Packet::ResourcePackDataInfo(info)] = packets.as_slice() else {
            panic!("one data info: {packets:?}");
        };
        let pack = &offer.packs()[0];
        assert_eq!(info.uuid, id);
        assert_eq!(info.data_chunk_size, CHUNK_BYTES);
        assert_eq!(info.chunk_count, 3);
        assert_eq!(info.size, pack.size_bytes());
        assert_eq!(info.hash, pack.sha256());
        assert_eq!(info.pack_type, u8::from(ResourcePackType::Resources));
        assert_eq!(extend, DOWNLOAD_BASE);

        let mut downloaded = Vec::new();
        for i in 0..3 {
            t.on_chunk_request(&offer, &chunk(&id, i)).unwrap();
            assert!(t.has_chunks());
            let Some(Packet::ResourcePackChunkData(data)) = t.next_chunk(&offer) else {
                panic!("chunk {i}");
            };
            assert_eq!(data.uuid, id);
            assert_eq!(data.chunk_index, u32::try_from(i).unwrap());
            assert_eq!(data.data_offset, u64::try_from(downloaded.len()).unwrap());
            downloaded.extend_from_slice(&data.data);
        }
        assert!(!t.has_chunks());
        assert_eq!(downloaded, pack.bytes());
        assert_eq!(
            crate::crypto::sha256(&[&downloaded]).as_slice(),
            info.hash.as_slice()
        );

        let (packets, _) = sent(
            t.on_response(&offer, &response(PackResponse::AllPacksDownloaded, &[]))
                .unwrap(),
        );
        assert!(
            matches!(&packets[..], [Packet::ResourcePackStack(s)] if s.texture_packs.len() == 2)
        );
        assert!(matches!(
            t.on_response(&offer, &response(PackResponse::Completed, &[])),
            Ok(Reply::Join)
        ));
    }

    #[test]
    fn bad_chunk_requests_close_the_session() {
        let offer = offer();
        let mut t = Transfer::default();
        let id = offer.id(0).to_owned();
        assert_eq!(
            t.on_chunk_request(&offer, &chunk(&id, 0)),
            Err(Error::Order("chunk of a pack that was not requested"))
        );
        sent(
            t.on_response(
                &offer,
                &response(PackResponse::SendPacks, &[UUID.to_owned()]),
            )
            .unwrap(),
        );
        for (request, error) in [
            (chunk(&id, 3), "chunk index out of range"),
            (chunk(&id, -1), "chunk index out of range"),
            (chunk(&id, i32::MAX), "chunk index out of range"),
            (
                chunk(offer.id(1), 0),
                "chunk of a pack that was not requested",
            ),
            (chunk("nonsense", 0), "chunk of a pack that was not offered"),
            (
                chunk(&format!("{UUID}_9.9.9"), 0),
                "chunk of a pack that was not offered",
            ),
        ] {
            assert_eq!(
                t.on_chunk_request(&offer, &request),
                Err(Error::Order(error))
            );
        }
        t.on_chunk_request(&offer, &chunk(&id, 1)).unwrap();
        assert_eq!(
            t.on_chunk_request(&offer, &chunk(&id, 1)),
            Err(Error::Order("chunk requested twice"))
        );
        // The name without a version is the same pack, so this is a repeat too.
        assert_eq!(
            t.on_chunk_request(&offer, &chunk(UUID, 1)),
            Err(Error::Order("chunk requested twice"))
        );
    }

    #[test]
    fn bad_pack_responses_close_the_session() {
        let offer = offer();
        let id = offer.id(0).to_owned();
        let mut t = Transfer::default();
        assert_eq!(
            t.on_response(
                &offer,
                &response(PackResponse::SendPacks, &["x".to_owned()])
            )
            .unwrap_err(),
            Error::Order("requested a pack that was not offered")
        );
        let mut t = Transfer::default();
        assert_eq!(
            t.on_response(
                &offer,
                &response(PackResponse::SendPacks, &[id.clone(), UUID.to_owned()])
            )
            .unwrap_err(),
            Error::Order("pack requested twice")
        );
        let mut t = Transfer::default();
        let three = vec![id.clone(); 3];
        assert_eq!(
            t.on_response(&offer, &response(PackResponse::SendPacks, &three))
                .unwrap_err(),
            Error::Limit("more packs requested than offered")
        );
        let mut t = Transfer::default();
        let (packets, extend) = sent(
            t.on_response(&offer, &response(PackResponse::SendPacks, &[]))
                .unwrap(),
        );
        assert!(packets.is_empty() && extend.is_zero());
        assert_eq!(
            t.on_response(&offer, &response(PackResponse::SendPacks, &[]))
                .unwrap_err(),
            Error::Order("packs requested twice")
        );
        let mut t = Transfer::default();
        sent(
            t.on_response(&offer, &response(PackResponse::AllPacksDownloaded, &[]))
                .unwrap(),
        );
        assert_eq!(
            t.on_response(&offer, &response(PackResponse::AllPacksDownloaded, &[]))
                .unwrap_err(),
            Error::Order("stack requested twice")
        );
        assert_eq!(
            t.on_response(&offer, &response(PackResponse::SendPacks, &[id]))
                .unwrap_err(),
            Error::Order("packs requested after the stack")
        );
        assert!(matches!(
            Transfer::default().on_response(&offer, &response(PackResponse::Refused, &[])),
            Ok(Reply::Refused)
        ));
    }

    #[test]
    fn a_pack_has_the_chunks_its_size_needs() {
        assert_eq!(chunk_count(crate::pack::MAX_PACK_BYTES), 2048);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(u64::from(CHUNK_BYTES)), 1);
        assert_eq!(chunk_count(u64::from(CHUNK_BYTES) + 1), 2);
    }
}
