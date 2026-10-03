package main

import (
	"context"
	"errors"
	"fmt"
	"math"
	"reflect"
	"slices"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/df-mc/go-nethernet"
	"github.com/pion/webrtc/v4"
)

// NetherNet segment facts. A data channel message is one segment: a header byte holding the number
// of segments still to come, then the payload (go-nethernet message.go).
const (
	// defaultSegmentSize is go-nethernet's segment payload limit (a=max-message-size 262144 minus the
	// header byte).
	defaultSegmentSize = 262143
	// maxSegments is the most segments a message can have: the header byte counts down from 255.
	maxSegments = math.MaxUint8 + 1
	// keptSegmentRecords is how many segment records of single-segment messages are written; every
	// segment of a segmented message is written.
	keptSegmentRecords = 200
)

// segmentConn reads and writes a go-nethernet Conn's data channels directly, so the bot sees every
// segment the server sends before reassembly and chooses its own segment size. It takes over the
// channels' message handlers from go-nethernet (reached through reflect and unsafe: a test tool
// only) and reassembles reliable messages exactly as go-nethernet's dataChannel.handleMessage does.
// Messages on UnreliableDataChannel are counted and dropped, as go-nethernet's Conn never reads them.
type segmentConn struct {
	conn        *nethernet.Conn
	reliable    *webrtc.DataChannel
	segmentSize int
	messages    chan []byte
	clientSent  atomic.Bool

	mu sync.Mutex
	// pending holds the segment records not yet written; the session goroutine writes them.
	pending []map[string]any
	// partial is the reliable message being reassembled, partialLens its segment payload lengths
	// and expect the segment count its last segment promised.
	partial     []byte
	partialLens []int
	expect      int
	keep        bool
	err         error

	segments, kept, dropped, unhooked           int
	maxPayload, segmented, unreliable           int
	firstBeforeClient                           bool
	nonLastPayloads                             map[int]int
	sentSegments, maxSentPayload, sentSegmented int
}

// hookSegments takes over the data channels of a freshly dialled conn. A message that reached
// go-nethernet's own handler before the takeover is still read from the conn and counted as unhooked.
func hookSegments(conn *nethernet.Conn, segmentSize int) (*segmentConn, error) {
	field := reflect.ValueOf(conn).Elem().FieldByName("channels")
	if !field.IsValid() || field.Kind() != reflect.Array || field.Len() != 2 {
		return nil, errors.New("go-nethernet Conn has no channels array of two")
	}
	channels := reflect.NewAt(field.Type(), unsafe.Pointer(field.UnsafeAddr())).Elem()
	s := &segmentConn{conn: conn, segmentSize: segmentSize, messages: make(chan []byte), nonLastPayloads: map[int]int{}}
	for i := range channels.Len() {
		wrapped := channels.Index(i)
		if wrapped.IsNil() {
			return nil, fmt.Errorf("go-nethernet channel %v is not open", i)
		}
		ch, ok := wrapped.Elem().FieldByName("DataChannel").Interface().(*webrtc.DataChannel)
		if !ok || ch == nil {
			return nil, fmt.Errorf("go-nethernet channel %v has no webrtc.DataChannel", i)
		}
		reliability := nethernet.MessageReliability(i)
		if !reliability.Valid(ch) {
			return nil, fmt.Errorf("go-nethernet channel %v is %q, not %q", i, ch.Label(), reliability.Parameters().Label)
		}
		if reliability == nethernet.MessageReliabilityReliable {
			s.reliable = ch
		}
		label := ch.Label()
		ch.OnMessage(func(msg webrtc.DataChannelMessage) { s.receive(reliability, label, msg.Data) })
		go s.drain(reliability, label)
	}
	return s, nil
}

// drain reads messages that go-nethernet reassembled before the handlers were replaced, until the
// conn closes. It records them as unhooked: their segment boundaries are lost.
func (s *segmentConn) drain(r nethernet.MessageReliability, label string) {
	for {
		b, err := s.conn.Receive(r)
		if err != nil {
			return
		}
		s.mu.Lock()
		s.unhooked++
		if !s.clientSent.Load() {
			s.firstBeforeClient = true
		}
		s.pending = append(s.pending, map[string]any{"type": "segment", "channel": label, "unhooked": true, "len": len(b)})
		s.mu.Unlock()
		if r == nethernet.MessageReliabilityReliable && !s.deliver(b) {
			return
		}
	}
}

// receive handles one data channel message on pion's read goroutine.
func (s *segmentConn) receive(r nethernet.MessageReliability, label string, b []byte) {
	msg, err := s.segment(r, label, b)
	if err != nil {
		s.mu.Lock()
		if s.err == nil {
			s.err = err
		}
		s.mu.Unlock()
		// Like go-nethernet: a bad segment is unrecoverable. pion holds a lock while calling the handler.
		go s.conn.Close()
		return
	}
	if msg != nil {
		s.deliver(msg)
	}
}

// deliver passes a whole reliable message to ReadPacket; it reports false once the conn is closed.
func (s *segmentConn) deliver(msg []byte) bool {
	select {
	case s.messages <- msg:
		return true
	case <-s.conn.Context().Done():
		return false
	}
}

// segment records one segment and returns the reliable message it completes, if any.
func (s *segmentConn) segment(r nethernet.MessageReliability, label string, b []byte) ([]byte, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.clientSent.Load() {
		s.firstBeforeClient = true
	}
	s.segments++
	if len(b) < 2 {
		s.pending = append(s.pending, map[string]any{"type": "segment", "channel": label, "raw_len": len(b)})
		return nil, fmt.Errorf("%v: segment of %v bytes", label, len(b))
	}
	remaining, payload := int(b[0]), len(b)-1
	s.maxPayload = max(s.maxPayload, payload)
	rec := map[string]any{"type": "segment", "channel": label, "remaining": remaining, "len": payload}
	if r == nethernet.MessageReliabilityUnreliable {
		s.unreliable++
		s.record(rec, remaining > 0)
		return nil, nil
	}
	if s.partialLens != nil && remaining != s.expect-1 {
		s.record(rec, true)
		return nil, fmt.Errorf("%v: segment promises %v more, want %v", label, remaining, s.expect-1)
	}
	if s.partialLens == nil {
		s.keep = remaining > 0
	}
	s.record(rec, s.keep)
	s.expect = remaining
	s.partialLens = append(s.partialLens, payload)
	if len(s.partial)+payload > maxDecompressed {
		return nil, fmt.Errorf("%v: message exceeds %v bytes", label, maxDecompressed)
	}
	s.partial = append(s.partial, b[1:]...)
	if remaining > 0 {
		return nil, nil
	}
	lens := s.partialLens
	if len(lens) > 1 {
		s.segmented++
		for _, n := range lens[:len(lens)-1] {
			s.nonLastPayloads[n]++
		}
	}
	msg := s.partial
	s.partial, s.partialLens = nil, nil
	return msg, nil
}

// record queues a segment record: always when keep is set, else only the first keptSegmentRecords.
func (s *segmentConn) record(rec map[string]any, keep bool) {
	if !keep && s.kept >= keptSegmentRecords {
		s.dropped++
		return
	}
	if !keep {
		s.kept++
	}
	s.pending = append(s.pending, rec)
}

// flushRecords writes the queued segment records.
func (s *segmentConn) flushRecords(out *output) {
	s.mu.Lock()
	pending := s.pending
	s.pending = nil
	s.mu.Unlock()
	for _, rec := range pending {
		out.line(rec)
	}
}

// ReadPacket returns the next whole reliable message.
func (s *segmentConn) ReadPacket() ([]byte, error) {
	select {
	case b := <-s.messages:
		return b, nil
	case <-s.conn.Context().Done():
		s.mu.Lock()
		err := s.err
		s.mu.Unlock()
		if err != nil {
			return nil, err
		}
		return nil, context.Cause(s.conn.Context())
	}
}

// Write sends b on ReliableDataChannel in segments of at most segmentSize payload bytes, as
// go-nethernet's Conn.Send does with its fixed 262,143.
func (s *segmentConn) Write(b []byte) (int, error) {
	s.clientSent.Store(true)
	total := max(1, (len(b)+s.segmentSize-1)/s.segmentSize)
	if total > maxSegments {
		return 0, fmt.Errorf("%v bytes need %v segments of %v (max %v)", len(b), total, s.segmentSize, maxSegments)
	}
	n := 0
	for i := range total {
		frag := b[i*s.segmentSize : min(len(b), (i+1)*s.segmentSize)]
		seg := make([]byte, 0, 1+len(frag))
		seg = append(seg, byte(total-1-i))
		seg = append(seg, frag...)
		if err := s.reliable.Send(seg); err != nil {
			return n, fmt.Errorf("write segment %v of %v: %w", i, total, err)
		}
		n += len(frag)
		s.mu.Lock()
		s.sentSegments++
		s.maxSentPayload = max(s.maxSentPayload, len(frag))
		s.mu.Unlock()
	}
	if total > 1 {
		s.mu.Lock()
		s.sentSegmented++
		s.mu.Unlock()
	}
	return n, nil
}

// Close closes the go-nethernet Conn.
func (s *segmentConn) Close() error {
	return s.conn.Close()
}

// BatchHeader is NetherNet's: none.
func (s *segmentConn) BatchHeader() []byte {
	return s.conn.BatchHeader()
}

// DisableEncryption is NetherNet's: no Minecraft encryption.
func (s *segmentConn) DisableEncryption() bool {
	return s.conn.DisableEncryption()
}

// summary adds the segment statistics to the result record.
func (s *segmentConn) summary(m map[string]any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	sizes := make([]int, 0, len(s.nonLastPayloads))
	for n := range s.nonLastPayloads {
		sizes = append(sizes, n)
	}
	slices.Sort(sizes)
	m["segment_size"] = s.segmentSize
	m["segments"] = s.segments
	m["max_segment_payload"] = s.maxPayload
	m["segmented_messages"] = s.segmented
	m["non_last_segment_payloads"] = sizes
	m["unreliable_messages"] = s.unreliable
	m["unhooked_messages"] = s.unhooked
	m["first_server_message_before_client"] = s.firstBeforeClient
	m["segment_records_dropped"] = s.dropped
	m["sent_segments"] = s.sentSegments
	m["max_sent_segment_payload"] = s.maxSentPayload
	m["sent_segmented_messages"] = s.sentSegmented
}
