package main

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"sync/atomic"
	"time"

	"github.com/go-jose/go-jose/v4"
	"github.com/go-jose/go-jose/v4/jwt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/login"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// maxDecompressed bounds one decompressed batch from the server.
const maxDecompressed = 64 << 20

// endMarker is the block whose placement anywhere ends the blocks scenario (cargo xtask p6 places it last).
const endMarker = "minecraft:diamond_block"

// result is the outcome of one scenario.
type result struct {
	pass, spawned, initialised, stayed    bool
	ended                                 bool
	reason, disconnect                    string
	protocolAccepted, encrypted           bool
	loginSent, handshake                  bool
	loginSize, decodeErrors, packets      int
	loginSuccessIndex, packsInfoIndex     int
	disconnectReason                      int
	disconnectMessage                     string
	transport, nethernetError, joinScheme string
	join                                  map[string]any
	answerMaxMessageSize                  string
	reader                                *batchReader
	segments                              *segmentConn
	blocks                                *blockCheck
}

func newResult() *result {
	return &result{loginSuccessIndex: -1, packsInfoIndex: -1, disconnectReason: -1}
}

func (r *result) json(c config) map[string]any {
	m := map[string]any{
		"type":                      "result",
		"scenario":                  c.scenario,
		"requested_transport":       c.transport,
		"transport":                 r.transport,
		"nethernet_error":           r.nethernetError,
		"join_scheme":               r.joinScheme,
		"join":                      r.join,
		"pass":                      r.pass,
		"reason":                    r.reason,
		"login_sent":                r.loginSent,
		"handshake":                 r.handshake,
		"disconnect_reason":         r.disconnectReason,
		"disconnect_message":        r.disconnectMessage,
		"client_protocol":           protocol.CurrentProtocol,
		"protocol_accepted":         r.protocolAccepted,
		"encrypted":                 r.encrypted,
		"login_size":                r.loginSize,
		"spawned":                   r.spawned,
		"initialised":               r.initialised,
		"stayed":                    r.stayed,
		"ended":                     r.ended,
		"disconnect":                r.disconnect,
		"decode_errors":             r.decodeErrors,
		"packets":                   r.packets,
		"login_success_index":       r.loginSuccessIndex,
		"resource_packs_info_index": r.packsInfoIndex,
	}
	if r.answerMaxMessageSize != "" {
		m["answer_max_message_size"] = r.answerMaxMessageSize
	}
	if r.reader != nil {
		r.reader.summary(m)
	}
	if r.segments != nil {
		r.segments.summary(m)
	}
	return m
}

// session is one client connection.
type session struct {
	c        config
	out      *output
	items    *output
	world    *output
	conn     packetConn
	reader   *batchReader
	enc      *packet.Encoder
	dec      *packet.Decoder
	pool     packet.Pool
	key      *ecdsa.PrivateKey
	shieldID int32
	res      *result
	blocks   *blockCheck

	runtimeID           uint64
	gotRadius, gotSpawn bool
	stayed              atomic.Bool

	// spawnChunk is the chunk of StartGame's player position (spawnKnown once StartGame arrived).
	spawnChunk protocol.ChunkPos
	spawnKnown bool
	// requested holds the chunks whose sub-chunks the bot requested.
	requested map[protocol.ChunkPos]bool
	// probeName labels the sent line of a probe request (chunk.go).
	probeName string
	// pendingAnswers counts the blocks scenario's last sub-chunk requests not answered yet.
	pendingAnswers int
	// endID is the network ID of endMarker.
	endID uint32
}

// play runs the scenario and returns its result. Failures to reach the server are failed results.
func play(ctx context.Context, c config, out, items, world *output) *result {
	res := newResult()
	key, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	if err != nil {
		res.reason = err.Error()
		return res
	}
	var token string
	if c.scenario == "online" {
		if token, err = mintToken(c, key); err != nil {
			res.reason = "mint token: " + err.Error()
			return res
		}
	}
	request, err := buildRequest(c, key, token)
	if err != nil {
		res.reason = "build login request: " + err.Error()
		return res
	}
	res.loginSize = len(request)

	conn, err := connect(ctx, c, out, key, token, res)
	if err != nil {
		res.reason = "connect: " + err.Error()
		return res
	}
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer stop()
	defer conn.Close()

	endID, err := networkBlockHash(endMarker, nil)
	if err != nil {
		res.reason = err.Error()
		return res
	}
	s := &session{
		c: c, out: out, items: items, world: world, conn: conn, key: key, res: res,
		pool:      packet.NewServerPool(),
		blocks:    newBlockCheck(),
		requested: map[protocol.ChunkPos]bool{},
		endID:     endID,
	}
	s.reader = newBatchReader(conn, out)
	res.reader, res.blocks = s.reader, s.blocks
	s.enc = packet.NewEncoder(conn)
	s.dec = packet.NewDecoder(s.reader)
	// The 812 packet limit is a server-side rule; the bot records larger batches instead of refusing them.
	s.dec.DisableBatchPacketLimit()

	if c.firstSendDelay > 0 {
		select {
		case <-ctx.Done():
		case <-time.After(c.firstSendDelay):
		}
	}
	if err := s.send(&packet.RequestNetworkSettings{ClientProtocol: protocol.CurrentProtocol}); err != nil {
		res.reason = err.Error()
		return res
	}
	var loopErr error
	for loopErr == nil {
		loopErr = s.dec.DecodeFunc(func(b []byte) error { return s.handle(b, request) })
	}
	if res.segments != nil {
		res.segments.flushRecords(out)
	}
	s.out.line(s.blocks.json())
	s.judge(ctx, loopErr)
	return res
}

// judge sets the pass flag and the reason once the connection ended with loopErr.
func (s *session) judge(ctx context.Context, loopErr error) {
	res := s.res
	// The server ended the connection: a read failed and neither -stay nor the time limit closed it.
	closedByServer := s.reader.readErr != nil && !s.stayed.Load() && ctx.Err() == nil
	switch s.c.scenario {
	case "kicked":
		res.pass = res.initialised && res.decodeErrors == 0 &&
			res.disconnectReason == int(packet.DisconnectReasonLoggedInOtherLocation)
		switch {
		case res.disconnect != "":
			res.reason = "disconnected after spawning: " + res.disconnect
		case s.stayed.Load():
			res.reason = fmt.Sprintf("stayed %v without a Disconnect", s.c.stay)
		default:
			res.reason = s.failure(ctx, loopErr)
		}
	case "rejected", "bind-mismatch":
		refused := res.disconnect != "" || (s.c.scenario == "bind-mismatch" && res.loginSent && closedByServer)
		res.pass = refused && !res.spawned
		switch {
		case res.spawned:
			res.reason = "spawned"
		case res.disconnect != "":
			res.reason = "disconnected before spawning: " + res.disconnect
		case closedByServer:
			res.reason = "the server closed the connection before spawning: " + s.reader.readErr.Error()
		default:
			res.reason = s.failure(ctx, loopErr)
		}
	case "blocks":
		res.pass = res.ended && res.disconnect == "" && res.decodeErrors == 0
		switch {
		case res.pass:
		case res.ended:
			res.reason = fmt.Sprintf("%v packets failed to decode", res.decodeErrors)
		case s.stayed.Load():
			res.reason = fmt.Sprintf("no %v placed within %v", endMarker, s.c.stay)
		default:
			res.reason = s.failure(ctx, loopErr)
		}
	default:
		if s.stayed.Load() && res.disconnect == "" {
			res.stayed = true
			res.pass = res.decodeErrors == 0
			if !res.pass {
				res.reason = fmt.Sprintf("%v packets failed to decode", res.decodeErrors)
			}
			return
		}
		res.reason = s.failure(ctx, loopErr)
	}
}

// failure describes why a connection ended early.
func (s *session) failure(ctx context.Context, loopErr error) string {
	switch {
	case s.res.disconnect != "":
		return "disconnected: " + s.res.disconnect
	case ctx.Err() != nil:
		return "timed out"
	default:
		return loopErr.Error()
	}
}

// send encodes the packets passed into one batch and records them.
func (s *session) send(pks ...packet.Packet) error {
	batch := make([][]byte, 0, len(pks))
	for _, pk := range pks {
		var buf bytes.Buffer
		h := packet.Header{PacketID: pk.ID()}
		if err := h.Write(&buf); err != nil {
			return err
		}
		pk.Marshal(protocol.NewWriter(&buf, s.shieldID))
		batch = append(batch, buf.Bytes())
		rec := map[string]any{
			"type": "sent", "name": packetName(pk), "id": pk.ID(), "size": buf.Len(),
			"after": s.res.packets - 1,
		}
		if _, ok := pk.(*packet.SubChunkRequest); ok {
			rec["fields"] = toJSON(reflect.ValueOf(pk), 0, false)
		}
		if s.probeName != "" {
			rec["probe"] = s.probeName
		}
		s.out.line(rec)
	}
	if err := s.enc.Encode(batch); err != nil {
		return fmt.Errorf("send %v: %w", packetName(pks[0]), err)
	}
	return nil
}

// handle decodes, records and reacts to one received packet.
func (s *session) handle(b []byte, request []byte) error {
	buf := bytes.NewBuffer(b)
	var h packet.Header
	if err := h.Read(buf); err != nil {
		return fmt.Errorf("read packet header: %w", err)
	}
	var pk packet.Packet
	if f, ok := s.pool[h.PacketID]; ok {
		pk = f()
	} else {
		pk = &packet.Unknown{PacketID: h.PacketID}
	}
	err := decode(pk, buf, s.shieldID)
	index := s.res.packets
	s.res.packets++
	rec := map[string]any{
		"type": "packet", "index": index, "batch": s.reader.batches - 1, "id": h.PacketID,
		"name": packetName(pk), "size": len(b), "fields": toJSON(reflect.ValueOf(pk), 0, false),
	}
	if err != nil {
		s.res.decodeErrors++
		rec["error"] = err.Error()
	}
	s.out.line(rec)
	full := func(o *output) {
		o.line(map[string]any{"index": index, "name": packetName(pk), "fields": toJSON(reflect.ValueOf(pk), 0, true)})
	}
	switch pk := pk.(type) {
	case *packet.ItemRegistry, *packet.CreativeContent, *packet.CraftingData:
		full(s.items)
	case *packet.LevelChunk, *packet.BiomeDefinitionList, *packet.JigsawStructureData,
		*packet.CurrentStructureFeature:
		full(s.world)
	case *packet.SubChunk:
		full(s.world)
		if err == nil {
			s.out.line(subChunkRecord(index, pk))
		}
	}
	if err != nil {
		return nil
	}
	return s.react(pk, index, request)
}

// decode reads pk from buf; gophertunnel's Reader panics on malformed input.
func decode(pk packet.Packet, buf *bytes.Buffer, shieldID int32) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("decode: %v", r)
		}
	}()
	pk.Marshal(protocol.NewReader(buf, shieldID, false))
	if buf.Len() != 0 {
		return fmt.Errorf("%v unread bytes", buf.Len())
	}
	return nil
}

func packetName(pk packet.Packet) string {
	if u, ok := pk.(*packet.Unknown); ok {
		return fmt.Sprintf("Unknown(%d)", u.PacketID)
	}
	return reflect.TypeOf(pk).Elem().Name()
}

// react mirrors the client side of gophertunnel's minecraft/conn.go login sequence.
func (s *session) react(pk packet.Packet, index int, request []byte) error {
	switch pk := pk.(type) {
	case *packet.NetworkSettings:
		alg, ok := packet.CompressionByID(pk.CompressionAlgorithm)
		if !ok {
			return fmt.Errorf("unknown compression algorithm %v", pk.CompressionAlgorithm)
		}
		s.res.protocolAccepted = true
		s.enc.EnableCompression(alg, int(pk.CompressionThreshold))
		s.dec.EnableCompression(alg, maxDecompressed)
		s.reader.compressed = true
		if err := s.send(&packet.Login{ClientProtocol: protocol.CurrentProtocol, ConnectionRequest: request}); err != nil {
			return err
		}
		s.res.loginSent = true
		return nil
	case *packet.ServerToClientHandshake:
		s.res.handshake = true
		if s.reader.noEncryption {
			// Like gophertunnel's client on such a transport: answer without encrypting.
			return s.send(&packet.ClientToServerHandshake{})
		}
		key, err := s.handshakeKey(pk.JWT)
		if err != nil {
			return err
		}
		s.enc.EnableEncryption(key)
		s.dec.EnableEncryption(key)
		s.reader.enableEncryption(key)
		s.res.encrypted = true
		return s.send(&packet.ClientToServerHandshake{})
	case *packet.PlayStatus:
		switch pk.Status {
		case packet.PlayStatusLoginSuccess:
			s.res.loginSuccessIndex = index
			return s.send(&packet.ClientCacheStatus{Enabled: false})
		case packet.PlayStatusPlayerSpawn:
			s.gotSpawn = true
			s.res.spawned = true
			return s.tryInitialise()
		default:
			return fmt.Errorf("login failed: play status %v", pk.Status)
		}
	case *packet.ResourcePacksInfo:
		s.res.packsInfoIndex = index
		return s.send(&packet.ResourcePackClientResponse{Response: packet.PackResponseAllPacksDownloaded})
	case *packet.ResourcePackStack:
		return s.send(&packet.ResourcePackClientResponse{Response: packet.PackResponseCompleted})
	case *packet.StartGame:
		s.runtimeID = pk.EntityRuntimeID
		s.spawnChunk, s.spawnKnown = chunkOf(pk.PlayerPosition)
		s.blocks.hashed = pk.UseBlockNetworkIDHashes
		s.blocks.startGameBlocks = len(pk.Blocks)
	case *packet.ItemRegistry:
		for _, item := range pk.Items {
			if item.Name == "minecraft:shield" {
				s.shieldID = int32(item.RuntimeID)
			}
		}
		return s.send(&packet.RequestChunkRadius{ChunkRadius: 16, MaxChunkRadius: 16})
	case *packet.ChunkRadiusUpdated:
		s.gotRadius = true
		return s.tryInitialise()
	case *packet.LevelChunk:
		return s.levelChunk(pk)
	case *packet.SubChunk:
		for _, e := range pk.SubChunkEntries {
			if payload, ok := e.RawPayload.Value(); ok && e.Result == protocol.SubChunkResultSuccess {
				s.blocks.subChunk(payload)
			}
		}
		return s.subChunkAnswered()
	case *packet.UpdateBlock:
		return s.blockChanged(pk.NewBlockRuntimeID)
	case *packet.UpdateSubChunkBlocks:
		for _, e := range append(pk.Blocks, pk.Extra...) {
			if err := s.blockChanged(e.BlockRuntimeID); err != nil {
				return err
			}
		}
	case *packet.Disconnect:
		s.res.disconnect = fmt.Sprintf("reason %v: %q", pk.Reason, pk.Message)
		s.res.disconnectReason, s.res.disconnectMessage = int(pk.Reason), pk.Message
		return errors.New("disconnected")
	}
	return nil
}

// tryInitialise sends SetLocalPlayerAsInitialised once both ChunkRadiusUpdated and PlayStatus(PlayerSpawn)
// arrived, flushes the output so a watcher of the file sees the spawn, then stays connected for -stay.
func (s *session) tryInitialise() error {
	if !s.gotRadius || !s.gotSpawn || s.res.initialised {
		return nil
	}
	s.res.initialised = true
	if err := s.send(&packet.SetLocalPlayerAsInitialised{EntityRuntimeID: s.runtimeID}); err != nil {
		return err
	}
	s.out.flush()
	time.AfterFunc(s.c.stay, func() {
		s.stayed.Store(true)
		_ = s.conn.Close()
	})
	return s.probe()
}

// blockChanged starts the end of the blocks scenario when the server placed the end marker after the bot
// spawned: the bot requests every sub-chunk of the 3x3 chunks around the spawn chunk (probe "after-blocks")
// and leaves once all are answered (subChunkAnswered).
func (s *session) blockChanged(id uint32) error {
	if s.c.scenario != "blocks" || !s.res.initialised || id != s.endID || s.res.ended {
		return nil
	}
	s.res.ended = true
	if !s.spawnKnown {
		return s.leave()
	}
	s.probeName = "after-blocks"
	defer func() { s.probeName = "" }()
	for dx := int32(-1); dx <= 1; dx++ {
		for dz := int32(-1); dz <= 1; dz++ {
			pk := &packet.SubChunkRequest{
				Position: protocol.SubChunkPos{s.spawnChunk.X() + dx, 0, s.spawnChunk.Z() + dz},
				Offsets:  allOffsets(),
			}
			if err := s.send(pk); err != nil {
				return err
			}
			s.pendingAnswers++
		}
	}
	return nil
}

// subChunkAnswered counts the answers to the blocks scenario's last requests and leaves after the last.
func (s *session) subChunkAnswered() error {
	if !s.res.ended || s.pendingAnswers == 0 {
		return nil
	}
	s.pendingAnswers--
	if s.pendingAnswers == 0 {
		return s.leave()
	}
	return nil
}

// leave ends the connection loop of a scenario that finished.
func (s *session) leave() error {
	s.stayed.Store(true)
	return errors.New("scenario finished")
}

// levelChunk checks the block palettes of a full chunk, or requests its sub-chunks when the server uses
// the sub-chunk request system (SubChunkLimit is set) and the chunk is one of the 3x3 around the spawn chunk.
func (s *session) levelChunk(pk *packet.LevelChunk) error {
	limit, requestMode := pk.SubChunkLimit.Value()
	if !requestMode {
		s.blocks.levelChunk(pk.RawPayload, int(pk.SubChunkCount))
		return nil
	}
	if !s.requestsSubChunks(pk.Position) || s.requested[pk.Position] {
		return nil
	}
	s.requested[pk.Position] = true
	// Overworld sub-chunks -4 to 19 around the chunk's sub-chunk 0, as dragonfly's ViewSubChunks reads them.
	var offsets []protocol.SubChunkOffset
	for y := -4; y < 20 && (limit < 0 || len(offsets) < int(limit)); y++ {
		offsets = append(offsets, protocol.SubChunkOffset{0, int8(y), 0})
	}
	return s.send(&packet.SubChunkRequest{
		Dimension: pk.Dimension,
		Position:  protocol.SubChunkPos{pk.Position.X(), 0, pk.Position.Z()},
		Offsets:   offsets,
	})
}

// saltClaims are the claims of the ServerToClientHandshake JWT.
type saltClaims struct {
	Salt string `json:"salt"`
}

// handshakeKey verifies the server's handshake JWT and derives the encryption key as
// gophertunnel's conn.go does: SHA-256(salt || ECDH(client key, server key)).
func (s *session) handshakeKey(token []byte) ([32]byte, error) {
	tok, err := jwt.ParseSigned(string(token), []jose.SignatureAlgorithm{jose.ES384})
	if err != nil {
		return [32]byte{}, fmt.Errorf("parse server token: %w", err)
	}
	x5u, _ := tok.Headers[0].ExtraHeaders["x5u"].(string)
	pub := new(ecdsa.PublicKey)
	if err := login.ParsePublicKey(x5u, pub); err != nil {
		return [32]byte{}, fmt.Errorf("parse server public key: %w", err)
	}
	var c saltClaims
	if err := tok.Claims(pub, &c); err != nil {
		return [32]byte{}, fmt.Errorf("verify server token: %w", err)
	}
	salt, err := base64.RawStdEncoding.DecodeString(strings.TrimRight(c.Salt, "="))
	if err != nil {
		return [32]byte{}, fmt.Errorf("decode salt: %w", err)
	}
	priv, err := s.key.ECDH()
	if err != nil {
		return [32]byte{}, err
	}
	remote, err := pub.ECDH()
	if err != nil {
		return [32]byte{}, err
	}
	secret, err := priv.ECDH(remote)
	if err != nil {
		return [32]byte{}, err
	}
	return sha256.Sum256(append(salt, secret...)), nil
}
