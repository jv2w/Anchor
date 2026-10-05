package main

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"slices"
	"strings"
	"testing"

	"github.com/df-mc/go-nethernet"
)

func TestSDPStructureKeepsLineKeysOnly(t *testing.T) {
	sdp := "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\na=msid-semantic: WMS\r\na=identity:abc\r\n" +
		"m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=candidate:1 1 udp 2 1.2.3.4 5 typ host\r\n" +
		"a=extmap-allow-mixed\r\n"
	want := []string{"v", "o", "a=msid-semantic", "a=identity", "m=application", "a=candidate", "a=extmap-allow-mixed"}
	if got := sdpStructure(sdp); !slices.Equal(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestIdentityRecordDecodesTheAnswerIdentity(t *testing.T) {
	key, _ := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	server, err := nethernet.GenerateServerIdentity(key, "self")
	if err != nil {
		t.Fatal(err)
	}
	assertion, _ := json.Marshal(map[string]string{"fingerprints": "eyJhbGciOiJFUzM4NCJ9..c2ln", "token": server.Token})
	outer, _ := json.Marshal(map[string]any{
		"assertion": string(assertion), "idp": map[string]string{"domain": "self", "protocol": "default"},
	})
	answer := "v=0\r\na=identity:" + base64.StdEncoding.EncodeToString(outer) +
		"\r\nm=application 1 UDP/DTLS/SCTP webrtc-datachannel\r\na=setup:active\r\n"

	rec := identityRecord(answer)
	if rec["error"] != nil || rec["session_level"] != true || rec["line"] != 1 {
		t.Fatalf("got %v", rec)
	}
	header, _ := rec["token_header"].(map[string]any)
	payload, _ := rec["token_payload"].(map[string]any)
	fingerprints, _ := rec["fingerprints_header"].(map[string]any)
	if header["alg"] != "ES384" || payload["cpk"] == nil || fingerprints["alg"] != "ES384" {
		t.Errorf("got header %v, payload %v, fingerprints header %v", header, payload, fingerprints)
	}
	if raw, _ := rec["token_header_raw"].(string); !strings.HasPrefix(raw, "{") {
		t.Errorf("raw header %q", raw)
	}
	if rec := identityRecord("v=0\r\nm=application 1 UDP/DTLS/SCTP webrtc-datachannel\r\n"); rec["error"] == nil {
		t.Error("an answer without a=identity was decoded")
	}
}

func TestOfferIdentityProvesTheLoginKeyExceptForBindMismatch(t *testing.T) {
	key, _ := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	for _, c := range []struct {
		scenario string
		same     bool
	}{{"spawn", true}, {"online", true}, {"bind-mismatch", false}} {
		identity, err := clientIdentity(config{scenario: c.scenario, iss: "https://issuer.test"}, key, "token")
		if err != nil {
			t.Fatal(err)
		}
		if same := identity.PrivateKey.Equal(key); same != c.same {
			t.Errorf("%v: offer key is the Login key: %v, want %v", c.scenario, same, c.same)
		}
	}
	online, _ := clientIdentity(config{scenario: "online", iss: "https://issuer.test"}, key, "token")
	if online.Token != "token" || online.Domain != "https://issuer.test" {
		t.Errorf("online identity %+v", online)
	}
}

func TestFlagsRejectTransportsAScenarioCannotUse(t *testing.T) {
	base := []string{"-addr", "127.0.0.1:1", "-out", "x"}
	for _, args := range [][]string{
		{"-transport", "tcp"},
		{"-scenario", "bind-mismatch"},
		{"-scenario", "bind-mismatch", "-transport", "auto"},
		{"-scenario", "idle", "-transport", "nethernet"},
	} {
		if _, err := parseFlags(append(slices.Clone(base), args...)); err == nil {
			t.Errorf("%v was accepted", args)
		}
	}
	for _, args := range [][]string{
		{},
		{"-scenario", "bind-mismatch", "-transport", "nethernet"},
		{"-scenario", "kicked", "-transport", "auto"},
		{"-scenario", "rejected"},
	} {
		if _, err := parseFlags(append(slices.Clone(base), args...)); err != nil {
			t.Errorf("%v: %v", args, err)
		}
	}
}

func TestSignalingAddrIsAddrUnlessGiven(t *testing.T) {
	base := []string{"-addr", "127.0.0.1:1", "-out", "x"}
	if c, err := parseFlags(base); err != nil || c.signalingAddr != "127.0.0.1:1" {
		t.Errorf("default: %q, %v", c.signalingAddr, err)
	}
	c, err := parseFlags(append(slices.Clone(base), "-signaling-addr", "127.0.0.1:2"))
	if err != nil || c.signalingAddr != "127.0.0.1:2" || c.addr != "127.0.0.1:1" {
		t.Errorf("given: %q, %q, %v", c.addr, c.signalingAddr, err)
	}
}
