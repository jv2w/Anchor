package main

import (
	"bytes"
	"compress/flate"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/coreos/go-oidc/v3/oidc"
	"github.com/go-jose/go-jose/v4"
	"github.com/go-jose/go-jose/v4/jwt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/login"
)

// The hashes BDS 1.26.52.3 sent in its flat-world sub-chunk palettes (cargo xtask p6 bds).
func TestNetworkBlockHashMatchesBDSPalette(t *testing.T) {
	for _, c := range []struct {
		state blockState
		want  int32
	}{
		{blockState{name: "minecraft:air"}, -604749536},
		{blockState{name: "minecraft:bedrock", props: map[string]any{"infiniburn_bit": uint8(0)}}, -173245189},
		{blockState{name: "minecraft:dirt"}, -2108756090},
		{blockState{name: "minecraft:grass_block"}, -567203660},
		{blockState{name: "minecraft:unknown"}, -2},
	} {
		got, err := networkBlockHash(c.state.name, c.state.props)
		if err != nil || int32(got) != c.want {
			t.Errorf("%v: got %v (%v), want %v", c.state.name, int32(got), err, c.want)
		}
	}
	if _, err := networkBlockHash("minecraft:x", map[string]any{"a": 1.5}); err == nil {
		t.Error("a float state was hashed")
	}
}

func TestInflateCountsBytesAfterTheFinalBlock(t *testing.T) {
	var buf bytes.Buffer
	w, _ := flate.NewWriter(&buf, flate.DefaultCompression)
	_, _ = w.Write(bytes.Repeat([]byte("anchor"), 100))
	_ = w.Close()
	for _, extra := range []int{0, 3} {
		in := append(bytes.Clone(buf.Bytes()), make([]byte, extra)...)
		out, trailing, err := inflate(in)
		if err != nil || len(out) != 600 || trailing != extra {
			t.Errorf("extra %v: got %v bytes, trailing %v, err %v", extra, len(out), trailing, err)
		}
	}
}

func TestWalkBatchReportsHighHeaderBits(t *testing.T) {
	// Two packets: header 0x01 (ID 1) and header 0x4001 (bit 14 set, varuint 0x81 0x80 0x01).
	batch := []byte{1, 0x01, 3, 0x81, 0x80, 0x01}
	count, high, err := walkBatch(batch)
	if err != nil || count != 2 || high != 1 {
		t.Errorf("got count %v, high %v, err %v", count, high, err)
	}
	if _, _, err := walkBatch([]byte{5, 1}); err == nil {
		t.Error("a truncated packet was accepted")
	}
}

func TestBigSkinRequestHasTheTargetSizeAndValidClientData(t *testing.T) {
	key, _ := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	c := config{addr: "127.0.0.1:19132", scenario: "big-skin", name: "Bot", loginSize: 2 << 20}
	request, err := buildRequest(c, key, "")
	if err != nil {
		t.Fatal(err)
	}
	if diff := math.Abs(float64(len(request) - c.loginSize)); diff > float64(c.loginSize)/100 {
		t.Errorf("request is %v bytes, want about %v", len(request), c.loginSize)
	}
	if _, _, _, err := login.Parse(request, nil); err != nil {
		t.Errorf("gophertunnel rejects the request: %v", err)
	}
}

func TestOnlineRequestPassesGophertunnelVerification(t *testing.T) {
	issuerKey, _ := rsa.GenerateKey(rand.Reader, 2048)
	path := filepath.Join(t.TempDir(), "key.pem")
	der := x509.MarshalPKCS1PrivateKey(issuerKey)
	if err := os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: "RSA PRIVATE KEY", Bytes: der}), 0o600); err != nil {
		t.Fatal(err)
	}
	c := config{addr: "127.0.0.1:19132", scenario: "online", name: "Bot", tokenKey: path, kid: "k1", iss: "https://issuer.test"}
	key, _ := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)

	token, err := mintToken(c, key)
	if err != nil {
		t.Fatal(err)
	}
	tok, err := jwt.ParseSigned(token, []jose.SignatureAlgorithm{jose.RS256})
	if err != nil || tok.Headers[0].KeyID != "k1" {
		t.Fatalf("token header: %v, %v", tok, err)
	}

	request, err := buildRequest(c, key, token)
	if err != nil {
		t.Fatal(err)
	}
	verifier := oidc.NewVerifier(c.iss, &oidc.StaticKeySet{PublicKeys: []crypto.PublicKey{&issuerKey.PublicKey}},
		&oidc.Config{ClientID: multiplayerAudience, SupportedSigningAlgs: []string{"RS256"}})
	identity, _, auth, err := login.Parse(request, verifier)
	if err != nil {
		t.Fatal(err)
	}
	if identity.XUID != onlineXUID || identity.DisplayName != "Bot" || !auth.PublicKey.Equal(&key.PublicKey) {
		t.Errorf("got identity %+v, key match %v", identity, auth.PublicKey.Equal(&key.PublicKey))
	}
}

func TestFullJSONKeepsLargeValuesAndNBTTypes(t *testing.T) {
	stack := protocol.ItemStack{NBTData: map[string]any{
		"b": uint8(1), "i": int32(1), "l": []any{int16(2)}, "c": map[string]any{"s": "x"},
	}}
	items := make([]protocol.ItemEntry, maxElems+1)
	short := toJSON(reflect.ValueOf(items), 0, false)
	if _, ok := short.(map[string]any)["sha256"]; !ok {
		t.Errorf("a long list was not hashed: %T", short)
	}
	if full, ok := toJSON(reflect.ValueOf(items), 0, true).([]any); !ok || len(full) != maxElems+1 {
		t.Errorf("full mode shortened a long list")
	}
	got, _ := json.Marshal(toJSON(reflect.ValueOf(stack), 0, true).(map[string]any)["NBTData"])
	want := `{"compound":{"b":{"byte":1},"c":{"compound":{"s":{"string":"x"}}},"i":{"int":1},"l":{"list":[{"short":2}]}}}`
	if string(got) != want {
		t.Errorf("NBT: got %s, want %s", got, want)
	}
	if got := itemsPath("dir/spawn.jsonl"); got != "dir/spawn.items.jsonl" {
		t.Errorf("items path %v", got)
	}
}
