package main

import (
	"bytes"
	"crypto/ecdsa"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	_ "embed"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	mathrand "math/rand/v2"
	"os"
	"time"

	"github.com/go-jose/go-jose/v4"
	"github.com/go-jose/go-jose/v4/jwt"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/login"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The default skin geometry and resource patch, copied from gophertunnel's minecraft package (MIT), which
// embeds them for its Dialer's default ClientData.
var (
	//go:embed skin_geometry.json
	skinGeometry []byte
	//go:embed skin_resource_patch.json
	skinResourcePatch []byte
)

// multiplayerAudience is the audience of the multiplayer token.
const multiplayerAudience = "api://auth-minecraft-services/multiplayer"

// onlineXUID is the XUID the online scenario's token claims.
const onlineXUID = "2535400000000001"

// buildRequest builds the Login connection request for the scenario.
func buildRequest(c config, key *ecdsa.PrivateKey) ([]byte, error) {
	data := clientData(c.addr, c.name)
	switch c.scenario {
	case "online":
		token, err := mintToken(c, key)
		if err != nil {
			return nil, err
		}
		chain, err := selfSignedChain(key)
		if err != nil {
			return nil, err
		}
		return login.Encode(chain, data, key, token, false), nil
	case "big-skin":
		return bigSkinRequest(c, data, key)
	}
	return offlineRequest(c, data, key), nil
}

func offlineRequest(c config, data login.ClientData, key *ecdsa.PrivateKey) []byte {
	identity := login.IdentityData{DisplayName: c.name, Identity: uuid.New().String()}
	return login.EncodeOffline(identity, data, key, false)
}

// clientData mirrors gophertunnel's defaultClientData (minecraft/dial.go).
func clientData(addr, name string) login.ClientData {
	d := login.ClientData{
		ServerAddress:       addr,
		ThirdPartyName:      name,
		DeviceOS:            protocol.DeviceAndroid,
		DefaultInputMode:    packet.InputModeTouch,
		CurrentInputMode:    packet.InputModeTouch,
		GameVersion:         protocol.CurrentVersion,
		ClientRandomID:      mathrand.Int64(),
		LanguageCode:        "en_GB",
		AnimatedImageData:   []login.SkinAnimation{},
		PersonaPieces:       []login.PersonaPiece{},
		PieceTintColours:    []login.PersonaPieceTintColour{},
		SelfSignedID:        uuid.New().String(),
		SkinID:              uuid.New().String(),
		SkinData:            base64.StdEncoding.EncodeToString(bytes.Repeat([]byte{0, 0, 0, 255}, 32*64)),
		SkinImageHeight:     32,
		SkinImageWidth:      64,
		SkinResourcePatch:   base64.StdEncoding.EncodeToString(skinResourcePatch),
		SkinGeometry:        base64.StdEncoding.EncodeToString(skinGeometry),
		SkinGeometryVersion: base64.StdEncoding.EncodeToString([]byte("0.0.0")),
	}
	d.DeviceID = d.ExpectedDeviceIDFormat().Generate()
	id := make([]byte, 8)
	_, _ = rand.Read(id)
	d.PlayFabID = hex.EncodeToString(id)
	return d
}

// bigSkinRequest builds an offline request of about c.loginSize bytes by adding a valid but unused geometry
// with many cubes to the skin geometry JSON. The cube count is calibrated from the size of two requests.
func bigSkinRequest(c config, data login.ClientData, key *ecdsa.PrivateKey) ([]byte, error) {
	build := func(cubes int) ([]byte, error) {
		geometry, err := paddedGeometry(cubes)
		if err != nil {
			return nil, err
		}
		data.SkinGeometry = base64.StdEncoding.EncodeToString(geometry)
		if err := data.Validate(); err != nil {
			return nil, fmt.Errorf("big skin client data: %w", err)
		}
		return offlineRequest(c, data, key), nil
	}
	const probe = 1000
	base, err := build(0)
	if err != nil {
		return nil, err
	}
	sample, err := build(probe)
	if err != nil {
		return nil, err
	}
	per := float64(len(sample)-len(base)) / probe
	cubes := 0
	if c.loginSize > len(base) {
		cubes = int(float64(c.loginSize-len(base)) / per)
	}
	return build(cubes)
}

// paddedGeometry returns gophertunnel's default geometry plus a geometry "geometry.anchor.padding" with
// the given number of cubes.
func paddedGeometry(cubes int) ([]byte, error) {
	var doc map[string]any
	if err := json.Unmarshal(skinGeometry, &doc); err != nil {
		return nil, err
	}
	list, ok := doc["minecraft:geometry"].([]any)
	if !ok {
		return nil, errors.New("default geometry has no minecraft:geometry list")
	}
	if cubes == 0 {
		return json.Marshal(doc)
	}
	cube := map[string]any{"origin": []int{-4, 0, -4}, "size": []int{8, 8, 8}, "uv": []int{0, 0}}
	all := make([]any, cubes)
	for i := range all {
		all[i] = cube
	}
	doc["minecraft:geometry"] = append(list, map[string]any{
		"description": map[string]any{
			"identifier": "geometry.anchor.padding", "texture_width": 64, "texture_height": 64,
		},
		"bones": []any{map[string]any{"name": "padding", "pivot": []int{0, 0, 0}, "cubes": all}},
	})
	return json.Marshal(doc)
}

// mintToken mints the multiplayer token of a test issuer: RS256 with the PEM RSA key, header kid, and the
// claims Anchor checks (PLAN.md 3 and 10.2).
func mintToken(c config, key *ecdsa.PrivateKey) (string, error) {
	b, err := os.ReadFile(c.tokenKey)
	if err != nil {
		return "", err
	}
	block, _ := pem.Decode(b)
	if block == nil {
		return "", fmt.Errorf("%v: no PEM block", c.tokenKey)
	}
	var rsaKey *rsa.PrivateKey
	if k, err := x509.ParsePKCS1PrivateKey(block.Bytes); err == nil {
		rsaKey = k
	} else if k, err := x509.ParsePKCS8PrivateKey(block.Bytes); err == nil {
		rsaKey, _ = k.(*rsa.PrivateKey)
	}
	if rsaKey == nil {
		return "", fmt.Errorf("%v: not an RSA private key", c.tokenKey)
	}
	signer, err := jose.NewSigner(jose.SigningKey{Algorithm: jose.RS256, Key: rsaKey},
		(&jose.SignerOptions{}).WithType("JWT").WithHeader("kid", c.kid))
	if err != nil {
		return "", err
	}
	now := time.Now()
	return jwt.Signed(signer).Claims(map[string]any{
		"iss":   c.iss,
		"aud":   multiplayerAudience,
		"iat":   now.Unix(),
		"nbf":   now.Unix(),
		"exp":   now.Add(time.Hour).Unix(),
		"xid":   onlineXUID,
		"xname": c.name,
		"cpk":   login.MarshalPublicKey(&key.PublicKey),
	}).Serialize()
}

// selfSignedChain returns a one-token legacy chain signed by key. login.Encode needs a chain to read the
// identity public key from; Anchor ignores the chain when a token is present.
func selfSignedChain(key *ecdsa.PrivateKey) (string, error) {
	pub := login.MarshalPublicKey(&key.PublicKey)
	signer, err := jose.NewSigner(jose.SigningKey{Algorithm: jose.ES384, Key: key},
		&jose.SignerOptions{ExtraHeaders: map[jose.HeaderKey]any{"x5u": pub}})
	if err != nil {
		return "", err
	}
	now := time.Now()
	tok, err := jwt.Signed(signer).Claims(map[string]any{
		"identityPublicKey": pub,
		"nbf":               now.Add(-time.Hour).Unix(),
		"exp":               now.Add(time.Hour).Unix(),
	}).Serialize()
	if err != nil {
		return "", err
	}
	b, err := json.Marshal(map[string]any{"chain": []string{tok}})
	return string(b), err
}
