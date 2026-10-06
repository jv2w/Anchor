package main

import (
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"errors"
	"flag"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"time"
)

// issuerCommand makes a test issuer for the online scenario:
//
//	bot issuer -dir DIR -kid K -iss ISS
//
// It writes DIR/issuer.pem (a new RSA 2048 private key, PKCS#1) and DIR/auth.json, an Anchor
// authorization cache (src/auth/document.rs) fetched now whose OpenID issuer is ISS and whose
// JWKS holds the key under K. A server started with DIR as its data directory accepts tokens that
// `bot -scenario online -token-key DIR/issuer.pem -kid K -iss ISS` mints.
func issuerCommand(args []string) error {
	fs := flag.NewFlagSet("bot issuer", flag.ContinueOnError)
	dir := fs.String("dir", "", "directory for issuer.pem and auth.json")
	kid := fs.String("kid", "", "key ID")
	iss := fs.String("iss", "", "issuer, exactly as tokens carry it")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 0 || *dir == "" || *kid == "" || *iss == "" {
		return errors.New("usage: bot issuer -dir DIR -kid K -iss ISS")
	}
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return err
	}
	keyPEM := pem.EncodeToMemory(&pem.Block{Type: "RSA PRIVATE KEY", Bytes: x509.MarshalPKCS1PrivateKey(key)})
	if err := os.WriteFile(filepath.Join(*dir, "issuer.pem"), keyPEM, 0o600); err != nil {
		return err
	}
	b64 := base64.RawURLEncoding.EncodeToString
	cache := map[string]any{
		"auth": map[string]any{"serviceUri": "https://issuer.invalid", "issuer": *iss},
		"openid": map[string]any{
			"issuer":   *iss,
			"jwks_uri": "https://issuer.invalid/.well-known/keys",
		},
		"jwks": map[string]any{"keys": []map[string]any{{
			"kty": "RSA",
			"kid": *kid,
			"use": "sig",
			"alg": "RS256",
			"n":   b64(key.N.Bytes()),
			"e":   b64(big.NewInt(int64(key.E)).Bytes()),
		}}},
		"fetched_at": time.Now().Unix(),
	}
	b, err := json.Marshal(cache)
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(*dir, "auth.json"), b, 0o600); err != nil {
		return fmt.Errorf("write auth.json: %w", err)
	}
	return nil
}
