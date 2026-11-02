---
paths:
  - "src/raknet.rs"
  - "src/raknet/**"
  - "src/nethernet.rs"
  - "src/nethernet/**"
  - "src/udp.rs"
  - "src/session.rs"
  - "src/session/**"
  - "src/login.rs"
  - "src/login/**"
  - "src/auth.rs"
  - "src/auth/**"
  - "src/crypto.rs"
  - "src/crypto/**"
  - "src/protocol/batch.rs"
  - "src/protocol/compression.rs"
  - "src/protocol/compression/**"
  - "src/tick/queue.rs"
  - "src/tick/spawn.rs"
---
# Network rules

## Common
- Carry both transports (RakNet, NetherNet) on the one session state machine (`session`). Keep login, encryption, compression, and limits in one place above the transports, independent of them.
- Open UDP on `server-port` only: one IPv4 socket and one IPv6-only socket, shared by both transports. Never give a transport a port of its own.
- One UDP task (`udp.rs`) owns both sockets, the RakNet driver, and the NetherNet driver, and hands each datagram to a transport by its first bytes. Use no locks and never copy a datagram to another task.
- Both drivers apply the commands sessions queued before they apply close flags and before they stop, so a session's last packets go out before its close.
- Skip per-datagram receive errors (Windows reports an ICMP port unreachable as WSAECONNRESET) and keep reading.
- Build every per-IP limit key with the one function `raknet::ip_key`; RakNet, signaling, and NetherNet peers share it.
- Keep NetherNet's TCP signaling.
- Never open a socket per NetherNet connection: every peer shares the `server-port` sockets.
- Always keep the RakNet code. With `raknet = false`, accept no RakNet connection but still answer server list pings.

## Sessions
- Decompress, decrypt, decode, and verify the login in one session task per connection (tasks run in parallel).
- Nobody waits on a full queue between a session and the tick: a session whose share of the tick input queue is full ends, and the tick drops a session whose outbound queue is full.
- Disconnect when a login sequence packet (RequestNetworkSettings, Login, ClientToServerHandshake, ResourcePackClientResponse, ResourcePackChunkRequest) comes out of order or fails to decode. Before Login, any other packet also disconnects. On an encrypted RakNet connection, a packet after the Login in its batch also disconnects.
- After Login and before the join, hold every other packet in the deferred queue and hand it to the tick in order after the join (accept and drop ClientCacheStatus).
- After the spawn, a packet that fails to decode drops only that packet, logged at warn. Charge dropped packets to the input budget too.
- Treat a packet whose ID only servers send as one that fails to decode.
- Never charge a packet the client repeats every frame (a right click held on a block) to the packet budget. Drop only its copies, and record in `.claude/docs/network.md` where that can change the tick's result.
- On a graceful shutdown, the session layer sends Disconnect (`shutdown-message`) to every session past NetworkSettings that has not joined, and to every session whose join the tick did not apply.
- A second login of the same player never renames the new one: it disconnects the existing player and admits the new one (a listener may cancel). Decide it at the tick's join, not right after login.

## Login and authentication
- Match gophertunnel `minecraft/protocol/login` and `minecraft/service` as the ground truth for login and authentication.
- Never trust ClientData content.
- For an unknown `kid`, fetch only the JWKS again, at most once per 5 minutes.
- When `data/auth.json` is corrupt, refuse to start and leave the file untouched.
- Write `data/identity.key` and `data/auth.json` only through `fs::write_atomic`; `identity.key` with `Access::OwnerOnly`.
- Test online login with a test issuer (an OpenID configuration and JWKS from its own RSA key) placed in `auth.json`. Never add a test-only entry point to the engine.

## NetherNet
- Match the pinned df-mc/go-nethernet (version in `xtask/go/go.mod`) as the ground truth, with Mojang's NetherNet onboarding guide as secondary evidence.
- Signaling (`nethernet/signaling.rs`) only routes and enforces limits; it never interprets the offer.
- Refuse a signaling request that carries both `Content-Length` and `Transfer-Encoding: chunked` with 400.
- Refuse to start when the `nethernet-tls-cert` or `nethernet-tls-key` PEM is invalid.
- Refuse an offer without an identity in online mode (code 37); accept it in offline mode.
- When the Login's client key differs from the `cpk` the offer proved, disconnect with NotAuthenticated, offline too.
- Put `a=identity` in every SDP answer.
- Write the server JWT's `cpk` as a JWK object, not as go-nethernet's base64 string.
- Keep the server identity key `data/identity.key` across restarts and create it only when it does not exist. When it exists but cannot be read or parsed, refuse to start, never create a new one, and leave the file untouched.
- Create one DTLS certificate at start and share it among all peers.
- Give str0m the remote ICE credentials, fingerprint, and `max-message-size` only through its SDP API, in an internal offer whose setup is `passive`; add remote candidates directly. Never send str0m's answer.
- `sdp.rs` writes the answer SDP the client gets, in the official server's layout.
- Send game messages only on the reliable channel, only once both channels are open.
- Each peer keeps its own deadlines; never leave them to str0m.
- Run every str0m call of a peer under `catch_unwind`, so a panic closes only that peer.
- When one UDP task is not enough, let one receiving task distribute datagrams and split the peers over several tasks.
- Measure peer cost with the `#[ignore]` tests `nethernet::peer::tests::driver_cost_per_peer` and `handshake_under_random_loss`.

## RakNet
- Implement only the server side of RakNet, in-house.
- Keep the sans-IO core (`raknet/wire.rs`, `raknet/peer.rs`, `raknet/listener.rs`) apart from the driver.
- The RakNet layer adds and removes the 0xfe byte; the session stays transport-independent.
- RakNet timing uses the monotonic clock (`clock::now`).
- Never use the connection cookie (security).
- Drop a malformed ACK or NAK, and a datagram whose number is the history (1,024) or more ahead, as that datagram alone. Never close the connection for them.
- Never preallocate for the fragment count a message declares.
- Refuse `;` and control characters in `motd` and `level-name`.

## Encryption
- Match gophertunnel `minecraft/conn.go` and `packet/encryption.go` as the ground truth for encryption.
- Keep every cryptographic primitive of Anchor's code (ES384, RS256, ECDH P-384, SHA-256, AES-256-CTR, randomness), base64, and the JWS compact form in the `crypto` module.

## Compression
- Check `compression-threshold`, `compression-level`, and `max-batch-bytes` in one place, `batch::Settings::new`.
- Never accept a compression threshold of 0.
- Server sends use the threshold announced in NetworkSettings.
- Accept only standard Snappy elements, never the S2 extension.
- Apply the decompression limit `max-batch-bytes` to zlib, Snappy, and uncompressed batches alike, before and after login. Never lower it before login.
- Snappy checks, before allocating, that the declared length is within the limit and within 22 times the input. zlib inflates into a buffer that grows only up to the limit.
- Encode and compress broadcasts and the start-game packets every player gets the same (`tick::Shared`: ItemRegistry, CreativeContent, CraftingData, and the rest) once, and share the result. Build the shared batches at the start of `run()`.
- Record the report of Snappy crashes on devices without AVX2 in the `Compression` rustdoc.

## What cannot be verified
- Design what only the official client shows to accept every case, and list it under the remaining risks in `.claude/docs/network.md`.

Background and detailed facts are in `.claude/docs/network.md`.
