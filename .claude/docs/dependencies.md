# External dependencies

Working rules: `.claude/rules/dependencies.md`.

The sources of block and item data (the dump source, the reference list, the upgrade schemas) are in `data.md`.

## Runtime dependencies

Versions and features are the `=` pins in `Cargo.toml` and the locked versions in `Cargo.lock`. Every "denied" entry point is in the deny lists of `.clippy.toml`.

| Crate | Use | Defects and measures |
|---|---|---|
| tokio (MIT) | Async runtime, bounded queues, UDP, signaling TCP, signals | Unbounded channels, `blocking_*` send and receive, unbounded io-util reads (`read_to_end`, `read_line`, `lines` and the like), `tokio::sync::Mutex` and `RwLock`, `block_on`, runtime builders, and its clock: denied |
| rayon (MIT/Apache-2.0) | CPU pool | Entry points that silently start the global pool (`join`, `scope`, `spawn`, parallel iterators and the like): denied. Dropping a pool does not wait for its threads → each thread reports its end through `exit_handler`, and `Pool::shutdown` waits for all of them (`src/runtime.rs`, `src/pool.rs`). A panic in a `spawn` job without a panic handler aborts the process → jobs run only through `Pool::map` (a parallel iterator inside `install`) and `Pool::map_in_turns` (`Scope::spawn` inside `in_place_scope`), where a panic reaches the caller (`a_panic_in_a_job_reaches_the_caller`, `src/pool.rs`) |
| aws-lc-rs (ISC AND (Apache-2.0 OR ISC)) | ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, randomness | The ECDSA public key parser also takes a bare SEC1 point and ignores bytes after the SPKI → `PublicKey::from_base64_spki` first requires exactly one DER SEQUENCE (`src/crypto.rs`). Its CTR encryption draws a random IV and restarts on every call → denied; `src/crypto/encryption.rs` keeps its own counter keystream. Signing and agreement keys are separate types → the key pair passes through PKCS#8, and `ServerKey::from_pkcs8` checks that both public keys are equal. Randomness and key generation entry points and `SystemRandom`: denied outside the crypto module |
| rustls (Apache-2.0/ISC/MIT), webpki-roots (CDLA-Permissive-2.0) | TLS of the auth fetch (through ureq), of signaling (server configuration), and the PEM reading of operator files | Panics when no provider is given and no process default is set → every builder takes the aws-lc-rs provider explicitly; the process-global provider entry points and `ServerConfig::builder`: denied. Why ring is in `Cargo.lock`: `.clippy.toml` |
| tokio-rustls (MIT/Apache-2.0) | TLS signaling | The pinned release avoids the 0.26.5 regression (tokio-rustls #204). It has no deadline of its own → used only inside the 10 s request deadline (`REQUEST_DEADLINE`, `src/nethernet/signaling.rs`). One request per connection → TLS 1.3 tickets and the session cache are off (`load_tls`) |
| ureq (MIT/Apache-2.0) | Fetching the auth documents, on the blocking pool | With a timeout, its default resolver starts a detached thread per DNS lookup → `InlineResolver` calls it without one, and the OS bounds DNS (`src/auth/fetch.rs`). The body limit also rejects a body of exactly the limit, so the largest accepted document is `MAX_DOCUMENT` (1 MiB) minus one byte. Blocking calls and unbounded body reads: denied |
| libdeflater (Apache-2.0) | zlib compression (measured 2 to 4 times faster than miniz_oxide at level 6) | The `use_rust_alloc` feature (also enabled by `freestanding`) writes through a null pointer when out of memory → default features only. `Decompressor` panics on an unknown libdeflate error → denied; miniz_oxide inflates. `Compressor::new` panics only when allocation fails, at creation, independent of external input |
| miniz_oxide (MIT/Zlib/Apache-2.0) | zlib inflation | `decompress_to_vec*` has no size limit → denied; Anchor's own loop grows the output only up to the limit. `compress_to_vec*`: denied, so libdeflate is the only compressor |
| serde, serde_json, toml (MIT/Apache-2.0) | Configuration and its in-place update (key and value spans of `toml::de::DeTable`, `src/config/update.rs`); JSON of the auth cache, JWTs, login, forms, pack manifests, and user data | toml stops at nesting depth 80 (its `unbounded` feature stays off). toml's error `Display` contains an `expect` → Anchor reads only `message()` and `span()`. `serde_json::from_reader` reads without a limit → denied. serde_json's default float parsing reads some written `f64` back as a different value (measured: about 43% of a million random bit patterns, about 6% of physics-like sequences), so user values reread from a checkpoint would differ and replay would diverge → `float_roundtrip` (no new crate; only float parsing is slower; `a_float_survives_a_checkpoint_round_trip`, `src/tick/task/data.rs`) |
| tracing (MIT) | Logging | Anchor leaves `attributes` off. Why syn 2 is still built: `.clippy.toml` |
| socket2 (MIT/Apache-2.0) | IPv6-only sockets, socket buffer sizes and the TCP signaling listener (`SO_REUSEADDR` on Unix, backlog) (`src/udp.rs`, `src/nethernet/signaling.rs`) | Blocking I/O entry points: denied. The one exception is the 503 `send` on a non-blocking socket (`refuse`, `src/nethernet/signaling.rs`) |
| memory-stats (MIT/Apache-2.0) | Resident memory for `/status` (Anchor has no `unsafe`, so it leaves the OS call to this crate) | The default Linux path reads the whole of `/proc/self/smaps` → `always_use_statm`. It reads a file → denied; called only on a blocking thread. Its windows-sys 0.52 is an allowed duplicate (reason in `.clippy.toml`) |
| redb (MIT/Apache-2.0) | Server data storage (copy `vendor/redb`) | Vendor copies below |
| str0m (MIT/Apache-2.0) | NetherNet WebRTC (copy `vendor/str0m`) | Vendor copies below |
| dimpl, sctp-proto (MIT/Apache-2.0) | DTLS 1.2 and SCTP under str0m (copies) | Vendor copies below |
| str0m-proto, str0m-aws-lc-rs, is (MIT/Apache-2.0) | Between str0m and dimpl (unchanged copies) | Vendor copies below |

## Implemented directly

| Subject | Reason |
|---|---|
| RakNet | The upstream (go-raknet) has many defects: CRC32 cookies, error blocking that lets a spoofed address lock out others, no deduplication of reliable indexes, 24-bit number overflow, quadratic reassembly, no congestion control, effectively unbounded queues |
| Snappy | The snap crate has `unsafe` on its external input path. The encoder follows golang/snappy's `encode.go` and `encode_other.go` (BSD-3-Clause; notice in `THIRD-PARTY-NOTICES.md`) |
| Signaling HTTP | httparse and hyper are larger than needed, and Anchor must set the limits anyway; a small implementation on tokio is enough |
| LevelDB reader (import) | rusty-leveldb writes into the source folder when it opens it (LOCK, log replay, compaction) and has `unsafe` and snap on its disk input path |
| Error types, CRC, base64, UUID, NBT, JWS compact form | Small, and easy to test against ground truth |
| MD5 (name-based version 3 UUIDs) | aws-lc-rs has none, and the input is a short public string, so speed and collision resistance do not matter |
| ZIP reading (resource packs) | Reads only what packs use (central directory, stored and DEFLATE entries), checking every bound |
| Deterministic math (`sin`, `cos`, `atan2` and the like) | std's transcendental functions may differ between platforms. `src/math.rs` uses only the four arithmetic operations, `sqrt`, `floor`, `ceil`, `round`, and `abs`, with fdlibm's coefficients |

## Vendor copies

- Each copy's `.cargo_vcs_info.json`, written by cargo when it packaged the crate, records the upstream commit.
- Why copies are `path` dependencies rather than `[patch]`, and not workspace members: the comment at `exclude` in `Cargo.toml`. `xtask/tests/downstream.rs` resolves a new project outside the repository and checks that every crate in `vendor/`, and every crate that names one in its API, resolves to the copy.
- `verify` runs each copy's own tests (`.claude/docs/testing.md`; `CHECKS` in `xtask/src/main.rs`). The dimpl run passes `/ignore:4099` to rust-lld, which ignores only that one linker warning. dimpl's tests build OpenSSL from source; on Windows that run gets Strawberry Perl as `OPENSSL_SRC_PERL` (`data.md`). dimpl's wolfssl tests run only outside Windows (a `cfg(not(windows))` dev-dependency).
- str0m's integration tests need seven capture files the published crate leaves out. `cargo xtask vendor-test str0m` downloads them from the copy's upstream commit, checks them against `xtask/data/vendor/str0m-test-data.sha256`, and runs every str0m test (`xtask/src/vendor.rs`).

### redb

| Fix | Defect | Reproduction test |
|---|---|---|
| 1 | While the cache stays under its size limit, the page cache queue grows by one entry per write to a cached page (#1517) → `lru_cache.rs` from upstream master `f95d4609` | `lru_cache::test::queue_stays_bounded_without_eviction` in the copy |
| 2 | The open reads the system tree without verifying checksums, so one damaged byte panics it (#1505, #1334) → the open verifies the system tree first and returns `Corrupted`. A debug build's walk of the data pages at open also verifies them first | `a_damaged_byte_is_an_error_not_a_panic` in `xtask/tests/redb_repro.rs`: a database after three commits, with the first byte of every 4 KiB page set wrong and 1,000 random byte flips; the published crate panics |
| 3 | Every growth doubles the file (or fills its trailing region, up to 4 GiB), so after `compact` any small commit doubles the file → grow by an eighth of the file, and at least twice the allocation | `a_small_commit_after_a_compaction_grows_the_file_by_a_step_not_double` in `xtask/tests/redb_repro.rs` |

- The copy also corrects the one upstream test expectation that fix 2 contradicts (`tests/check_integrity_failure.rs`), and its `.gitignore` keeps the published `Cargo.lock`.
- Limits: a crafted file whose slot checksums also match allocates up to 4 GiB (#1503; random damage does not reach it). Page damage that appears while the server runs can panic a read (#1334): the save thread panics, the server exits, and the next start refuses the file.
- Outside tests, redb starts no threads and reads no clock or randomness. Anchor lowers its cache (`CACHE_BYTES`, `src/storage.rs`). Its open, transaction, integrity check, compact, and commit entry points are denied outside the storage module.
- redb has no integrity check that leaves the file untouched (the open and `check_integrity` write the header; a repair also writes the allocator state); the start check works around it (`storage.md`).

### str0m

| Fix | Defect | Reproduction test |
|---|---|---|
| 1 | SCTP RTO.Initial is RFC 4960's 3 s, so a lost INIT waits 3 s (at 10% loss some setups passed the 10 s open deadline) → RFC 9260's 1 s (`sctp::RTO_INITIAL_MS`) | `a_lost_sctp_init_is_sent_again_after_one_second` in `src/nethernet/peer.rs` |
| 2 | The client SCTP association announces 65,535 inbound streams, so a remote writing on many stream IDs piles up streams and entries, all scanned on every poll → 16 (`sctp::MAX_INBOUND_STREAMS`; NetherNet uses channel IDs 1 and 3) | `sctp::tests::many_remote_streams_stay_bounded` in the copy |
| 3 | Dropping ChannelData for an unknown stream logs at warn, so a remote can fill the log → debug | none |

- Guards outside the copy: the default configuration uses the process-global provider, ICE credentials from fastrand, and a certificate per peer; those entry points, and the setters that assert or turn off fingerprint verification, are denied.
- The direct API cannot learn the remote `max-message-size`, so str0m's SDP API gets a `setup:passive` offer built by Anchor, and Anchor writes the answer the client receives (`src/nethernet/peer.rs`).
- ICE only reports disconnection and SCTP retransmits forever → each peer has a 10 s open deadline and a 10 s idle limit (`OPEN_TIMEOUT`, `IDLE_TIMEOUT`).
- `Rtc::accepts` is linear → peers are indexed by ufrag and address (`src/nethernet.rs`).
- Any panic left inside str0m and the crates under it is caught per peer with `catch_unwind` and closes only that peer.
- The `pii` feature is off. `is` logs the local and remote ICE credentials (ufrag and password) at debug through `Pii`, which redacts only with `pii` on (`vendor/is/src/agent.rs`), so they appear unredacted in debug logs.

### dimpl

| Fix | Defect |
|---|---|
| 1 | Two records of the same epoch before ChangeCipherSpec trip an assert → the second is dropped like a replay |
| 2 | A peer certificate larger than the poll buffer (str0m's 2,000 bytes) trips an assert → returns `BufferTooSmall`, and str0m closes with an error |
| 3 | A truncated `supported_versions` in Auto mode reads out of bounds → length check |

- Reproduction tests: `xtask/tests/dimpl_repro.rs`; each fails on the published crate.
- Upstream has a release `assert!` on returning a buffer from outside the pool; no path returns one.
- When the records inside one datagram arrive reordered, the copy drops that datagram and recovers by retransmission.

### sctp-proto

| Fix | Defect |
|---|---|
| 1 | With a zero receive window, missing chunks are still accepted without limit → only the chunk right after the cumulative TSN (RFC 4960 6.2) |
| 2 | The received-TSN record holds data already read and does not count it against the window → keeps only TSNs and subtracts every record from the window |
| 3 | Stream count negotiation is reversed (INIT and INIT-ACK) → as RFC 4960 5.1.1. Streams beyond the negotiated count are never created; their DATA has its TSN acknowledged and its data dropped (6.5). A reopened stream is queued once |
| 4 | Every new INIT from the same remote adds an association → one association per remote |
| 5 | Empty DATA is kept → the association ends (6.2) |
| 6 | SACK gap offsets overflow `u16` and TSN `+ 1` overflows (a debug panic) → wrapping arithmetic, gap block loop included |
| 7 | Every chunk sent or received re-sorts the whole queue (quadratic cost) → inserted where it sorts |
| 8 | Every poll walks all chunks in flight looking for retransmissions → skipped when no T3 timeout marked any |

- Reproduction tests: `association/anchor_test.rs` and `endpoint/anchor_test.rs` in the copy.
- Remaining: the SSN sort comparator is not a total order (not reproduced in about 4 million runs of the way it is used; a panic would close only that peer). An INIT or ERROR chunk during an association closes it (an RFC violation; only that peer disconnects). `Endpoint::connect` reads the clock.

### str0m-proto, str0m-aws-lc-rs, is

Copies with unchanged code. Without them the registry releases come along, dimpl appears twice (the defective original among them), and the types do not match. Each is identical to the published crate except for its `path` lines and the str0m repository's `LICENSE-MIT.txt`, which the published crate lacks.

## Development tools (Go, `xtask/go`)

Not part of Anchor's build. `xtask/go/go.mod` pins the versions.

| Module | Use | Defects and measures |
|---|---|---|
| sandertv/gophertunnel (MIT) | Protocol ground truth, extractor, bot | Comments disagree with code. NBT maps are written in random order → the test tools write them sorted. Login checks are loose (unanchored regular expressions, blocking DNS, random offline UUIDs, loose base64) → Anchor implements them strictly (`protocol.md`, `network.md`) |
| sandertv/go-raknet, df-mc/go-nethernet | Bot only | go-nethernet prefixes candidate lines twice and sends before the channel-open acknowledgement (how Anchor accepts both: `network.md`). go-nethernet v1.0.25's `SelfSignedIdentity` writes `cpk` as a JWK, and the official server rejects that client offer with 37 (`ErrorCodeIdentityNotAllowed`) → the pinned later release (base64 SPKI) |
| go-jose, go-oidc | Bot tokens | Development tool |
| go-gl/mathgl, google/uuid, pion/webrtc | Bot | Development tool |
| df-mc/goleveldb (BSD-2-Clause) | Cross-check of the import reader (`xtask/go/leveldbdump`) | Has no block compression type 2 (zlib with header), and writes into the folder when it opens a database → it opens a copy |
| golang.org/x/tools (BSD-3-Clause) | Type information for the extractor | A version that does not change gophertunnel's dependency versions |

The sources and SHA-256 of binary tools (the dump PHP, Strawberry Perl, the official server) are in `data.md`.

## Build time

The settings in `Cargo.toml` and `.cargo/config.toml` carry their reasons in comments. Not used:
- Dependency `opt-level = 1`: it makes the build slower.
- sccache: no effect, because the target path is part of its cache key and it does not cache proc-macros, build scripts, or linking.

The settings recommended to projects that use Anchor, with their measurements, are in the crate documentation's "Build, start and stop times" (`src/lib.rs`). Changing release `incremental` or `codegen-units` is not recommended: it changes inlining and can cost tick performance. Most of the time of a release rebuild of Anchor is the front end and code generation of the anchor crate alone.
