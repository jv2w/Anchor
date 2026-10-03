package main

import (
	"bytes"
	"slices"
	"testing"

	"github.com/df-mc/go-nethernet"
)

func newTestSegmentConn() *segmentConn {
	return &segmentConn{segmentSize: defaultSegmentSize, nonLastPayloads: map[int]int{}}
}

func seg(remaining byte, payload []byte) []byte {
	return append([]byte{remaining}, payload...)
}

func TestSegmentsReassembleAndCountLikeGoNethernet(t *testing.T) {
	s := newTestSegmentConn()
	const reliable = nethernet.MessageReliabilityReliable
	if msg, err := s.segment(reliable, "ReliableDataChannel", seg(0, []byte("one"))); err != nil || string(msg) != "one" {
		t.Fatalf("single segment: %q, %v", msg, err)
	}
	a, b, c := bytes.Repeat([]byte{1}, 5), bytes.Repeat([]byte{2}, 5), []byte{3}
	for i, part := range [][]byte{seg(2, a), seg(1, b)} {
		if msg, err := s.segment(reliable, "ReliableDataChannel", part); err != nil || msg != nil {
			t.Fatalf("segment %v: %q, %v", i, msg, err)
		}
	}
	msg, err := s.segment(reliable, "ReliableDataChannel", seg(0, c))
	if err != nil || !bytes.Equal(msg, slices.Concat(a, b, c)) {
		t.Fatalf("last segment: %v, %v", msg, err)
	}
	if _, err := s.segment(nethernet.MessageReliabilityUnreliable, "UnreliableDataChannel", seg(0, []byte("u"))); err != nil {
		t.Fatal(err)
	}
	m := map[string]any{}
	s.summary(m)
	if m["segments"] != 5 || m["max_segment_payload"] != 5 || m["segmented_messages"] != 1 ||
		m["unreliable_messages"] != 1 || m["first_server_message_before_client"] != true {
		t.Errorf("summary %v", m)
	}
	if got := m["non_last_segment_payloads"].([]int); !slices.Equal(got, []int{5}) {
		t.Errorf("non-last payloads %v", got)
	}
	if len(s.pending) != 5 {
		t.Errorf("%v records, want 5", len(s.pending))
	}
}

func TestSegmentsRejectWrongCountsAndEmptySegments(t *testing.T) {
	s := newTestSegmentConn()
	const reliable = nethernet.MessageReliabilityReliable
	if _, err := s.segment(reliable, "ReliableDataChannel", seg(2, []byte("a"))); err != nil {
		t.Fatal(err)
	}
	if _, err := s.segment(reliable, "ReliableDataChannel", seg(0, []byte("b"))); err == nil {
		t.Error("a segment that skips a count was accepted")
	}
	if _, err := newTestSegmentConn().segment(reliable, "ReliableDataChannel", []byte{0}); err == nil {
		t.Error("a segment without payload was accepted")
	}
}

func TestSegmentRecordsKeepTheFirstAndEverySegmentedMessage(t *testing.T) {
	s := newTestSegmentConn()
	const reliable = nethernet.MessageReliabilityReliable
	for range keptSegmentRecords + 10 {
		if _, err := s.segment(reliable, "ReliableDataChannel", seg(0, []byte("x"))); err != nil {
			t.Fatal(err)
		}
	}
	for _, part := range [][]byte{seg(1, []byte("a")), seg(0, []byte("b"))} {
		if _, err := s.segment(reliable, "ReliableDataChannel", part); err != nil {
			t.Fatal(err)
		}
	}
	if len(s.pending) != keptSegmentRecords+2 || s.dropped != 10 {
		t.Errorf("%v records, %v dropped", len(s.pending), s.dropped)
	}
}

func TestFlagsBoundTheSegmentSize(t *testing.T) {
	base := []string{"-addr", "127.0.0.1:1", "-out", "x"}
	for _, size := range []string{"0", "262144"} {
		if _, err := parseFlags(append(slices.Clone(base), "-segment-size", size)); err == nil {
			t.Errorf("-segment-size %v was accepted", size)
		}
	}
	c, err := parseFlags(append(slices.Clone(base), "-segment-size", "65535"))
	if err != nil || c.segmentSize != 65535 {
		t.Errorf("-segment-size 65535: %v, %v", c.segmentSize, err)
	}
	if c, _ := parseFlags(base); c.segmentSize != defaultSegmentSize {
		t.Errorf("default segment size %v", c.segmentSize)
	}
}
