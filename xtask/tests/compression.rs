//! Batch compression against gophertunnel (the batch part of P3, and P4): zlib and Snappy bodies
//! cross-decompress in both directions, and malformed or oversized bodies are errors.

// Test-only crate: its helpers are test code too (clippy `allow-*-in-tests`).
#![cfg(test)]

#[path = "support/oracle.rs"]
mod oracle;
#[path = "support/rng.rs"]
mod rng;

use anchor::protocol::batch::{self, Decoder, Encoder, Settings};
use anchor::protocol::compression::{self, Compression};
use oracle::Record;
use rng::Rng;

const MIB: usize = 1024 * 1024;
/// The default `max-batch-bytes`.
const MAX_BATCH_BYTES: usize = 16 * MIB;
const SIZES: [usize; 6] = [0, 1, 255, 256, 64 * 1024, MIB];
const ALGORITHMS: [(Compression, &str); 2] =
    [(Compression::Zlib, "zlib"), (Compression::Snappy, "snappy")];

/// Settings that compress every non-empty payload: threshold 1, the smallest allowed.
fn settings(compression: Compression) -> Settings {
    Settings::new(compression, 1, 6, MAX_BATCH_BYTES).expect("valid settings")
}

/// The batch header byte of a compressed body, which equals the `NetworkSettings` id.
fn header(compression: Compression) -> u8 {
    u8::try_from(compression.network_id()).expect("ids are 0 and 1")
}

/// Random bytes, or runs and repeated phrases when `repetitive`.
fn data(len: usize, repetitive: bool) -> Vec<u8> {
    let mut rng = Rng(0x0dd_ba11 + u64::try_from(len).expect("fits u64"));
    let mut out = Vec::with_capacity(len + 64);
    while out.len() < len {
        if repetitive {
            match rng.below(3) {
                0 => out.extend(std::iter::repeat_n(rng.byte() % 4, rng.below(256))),
                1 => out.extend_from_slice(b"minecraft:stone"),
                _ => out.extend((0..rng.below(8)).map(|_| rng.byte())),
            }
        } else {
            out.push(rng.byte());
        }
    }
    out.truncate(len);
    out
}

/// Every size with random and repetitive data, as records with ID 0.
fn inputs() -> Vec<Record> {
    SIZES
        .iter()
        .flat_map(|&len| [(0, data(len, false)), (0, data(len, true))])
        .collect()
}

#[test]
fn rust_compressed_bodies_decompress_in_gophertunnel() {
    for (compression, algo) in ALGORITHMS {
        let mut encoder = Encoder::new(settings(compression));
        let mut body = Vec::new();
        // An empty payload is always sent uncompressed (threshold ≥ 1), so the public API
        // cannot compress it; the Go→Rust direction covers size 0.
        let originals: Vec<Record> = inputs()
            .into_iter()
            .filter(|(_, input)| !input.is_empty())
            .collect();
        let compressed: Vec<Record> = originals
            .iter()
            .map(|(_, input)| {
                encoder.encode(input, &mut body).expect("compresses");
                let (&first, rest) = body.split_first().expect("a header byte");
                assert_eq!(first, header(compression), "{algo} len {}", input.len());
                (0, rest.to_vec())
            })
            .collect();
        let name = format!("rust-to-go-{algo}");
        let results = oracle::run(
            &name,
            &["decompress", "-algo", algo],
            Some(compressed.as_slice()),
        );
        assert_eq!(results.len(), originals.len());
        for ((_, input), (status, output)) in originals.iter().zip(&results) {
            assert_eq!(
                *status,
                0,
                "{algo} len {}: gophertunnel: {}",
                input.len(),
                String::from_utf8_lossy(output)
            );
            assert!(
                output == input,
                "{algo} len {}: output differs",
                input.len()
            );
        }
    }
}

#[test]
fn gophertunnel_compressed_bodies_decompress_in_rust() {
    let originals = inputs();
    for (compression, algo) in ALGORITHMS {
        let name = format!("go-to-rust-{algo}");
        let results = oracle::run(
            &name,
            &["compress", "-algo", algo],
            Some(originals.as_slice()),
        );
        assert_eq!(results.len(), originals.len());
        let mut decoder = Decoder::new(settings(compression));
        let mut output = Vec::new();
        for ((_, input), (status, compressed)) in originals.iter().zip(&results) {
            assert_eq!(*status, 0, "{algo} len {}: gophertunnel", input.len());
            let mut body = vec![header(compression)];
            body.extend_from_slice(compressed);
            decoder
                .decode(&body, &mut output)
                .unwrap_or_else(|e| panic!("{algo} len {}: {e:?}", input.len()));
            assert!(
                output == *input,
                "{algo} len {}: output differs",
                input.len()
            );
        }
    }
}

fn decode(compression: Compression, body: &[u8]) -> batch::Result<Vec<u8>> {
    let mut output = Vec::new();
    Decoder::new(settings(compression)).decode(body, &mut output)?;
    Ok(output)
}

fn snappy(body: &[u8]) -> batch::Result<Vec<u8>> {
    let mut framed = vec![header(Compression::Snappy)];
    framed.extend_from_slice(body);
    decode(Compression::Snappy, &framed)
}

fn compression_error(error: compression::Error) -> batch::Result<Vec<u8>> {
    Err(batch::Error::Compression(error))
}

#[test]
fn malformed_snappy_bodies_are_errors() {
    use compression::Error::{OffsetOutOfRange, Overrun, TrailingInput, Underrun, ZeroOffset};
    // Declared length, then elements: literal tag (len - 1) << 2, copy-2 tag (len - 1) << 2 | 2
    // with a u16 offset.
    assert_eq!(
        snappy(&[5, 0x00, b'a', 0x0e, 0, 0]),
        compression_error(ZeroOffset)
    );
    assert_eq!(
        snappy(&[5, 0x00, b'a', 0x0e, 2, 0]),
        compression_error(OffsetOutOfRange)
    );
    assert_eq!(snappy(&[1, 0x04, b'a', b'b']), compression_error(Overrun));
    assert_eq!(snappy(&[3, 0x00, b'a']), compression_error(Underrun));
    assert_eq!(
        snappy(&[1, 0x00, b'a', 0x00, b'b']),
        compression_error(TrailingInput)
    );
    assert_eq!(snappy(&[5, 0x00, b'a', 0x0e, 1, 0]), Ok(b"aaaaa".to_vec()));
}

#[test]
fn snappy_declared_lengths_over_the_limits_are_size_errors() {
    // 16 MiB + 1 declared: over max-batch-bytes even if the input were large enough.
    let mut body = vec![0x81, 0x80, 0x80, 0x08];
    body.resize(MIB, 0);
    assert_eq!(
        snappy(&body),
        compression_error(compression::Error::Size {
            limit: MAX_BATCH_BYTES
        })
    );
    // 1 MiB declared by a 5-byte body: over 22 × the compressed size, refused before allocating.
    let body = [0x80, 0x80, 0x40, 0x00, b'a'];
    assert_eq!(
        snappy(&body),
        compression_error(compression::Error::Size { limit: 5 * 22 })
    );
}

#[test]
fn bodies_inflating_past_max_batch_bytes_are_size_errors() {
    let zeros = vec![0; MAX_BATCH_BYTES + 1];
    for compression in [Compression::Zlib, Compression::Snappy] {
        let big = Settings::new(compression, 1, 6, 64 * MIB).expect("valid settings");
        let mut body = Vec::new();
        Encoder::new(big)
            .encode(&zeros, &mut body)
            .expect("compresses");
        assert!(body.len() < MIB, "{compression:?}: the body is a bomb");
        assert_eq!(
            decode(compression, &body),
            compression_error(compression::Error::Size {
                limit: MAX_BATCH_BYTES
            }),
            "{compression:?}"
        );
    }
    let mut uncompressed = vec![0xff];
    uncompressed.extend_from_slice(&zeros);
    assert_eq!(
        decode(Compression::Zlib, &uncompressed),
        compression_error(compression::Error::Size {
            limit: MAX_BATCH_BYTES
        })
    );
}
