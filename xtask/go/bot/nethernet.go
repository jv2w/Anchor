package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/df-mc/go-nethernet"
	"github.com/df-mc/go-nethernet/endpoint"
)

// Signaling caps: one HTTP request and the bytes of its body that are kept.
const (
	signalingRequestTimeout = 5 * time.Second
	maxJoinBody             = 64 << 10
)

// quiet drops go-nethernet's logs; the bot records what it needs itself.
var quiet = slog.New(slog.DiscardHandler)

// signalingClient is the HTTP client for /v1/join. It accepts any certificate: a server with its own
// certificate is trusted on first use like a plain HTTP one (PLAN.md 2.1).
func signalingClient() *http.Client {
	return &http.Client{
		Timeout: signalingRequestTimeout,
		Transport: &http.Transport{
			TLSClientConfig:   &tls.Config{InsecureSkipVerify: true},
			DisableKeepAlives: true,
		},
	}
}

// dialNetherNet pings /v1/join over https, then http, on -signaling-addr and dials the first that answers with
// go-nethernet's Dialer, recording each ping ("join"), the offer and answer ("sdp") and the server
// identity ("identity").
func dialNetherNet(ctx context.Context, c config, key *ecdsa.PrivateKey, token string, out *output, res *result) (*nethernet.Conn, error) {
	client := signalingClient()
	base, err := pingJoin(ctx, client, c.signalingAddr, out, res)
	if err != nil {
		return nil, err
	}
	identity, err := clientIdentity(c, key, token)
	if err != nil {
		return nil, fmt.Errorf("make the offer identity: %w", err)
	}
	signaling := &capture{Signaling: endpoint.ClientConfig{HTTPClient: client, Logger: quiet}.New()}
	conn, err := nethernet.Dialer{DisableTrickleICE: true, Identity: identity, Log: quiet}.DialContext(ctx, base, signaling)
	res.answerMaxMessageSize = signaling.record(out)
	if err != nil {
		return nil, fmt.Errorf("dial %v: %w", base, err)
	}
	return conn, nil
}

// pingJoin sends GET /v1/join over https, then http, and returns the base URL of the first answer that
// is a 2xx JSON object.
func pingJoin(ctx context.Context, client *http.Client, addr string, out *output, res *result) (string, error) {
	var errs []error
	for _, scheme := range []string{"https", "http"} {
		base := scheme + "://" + addr
		rec := map[string]any{"type": "join", "scheme": scheme, "url": base + "/v1/join"}
		status, err := getJoin(ctx, client, base, rec)
		if err != nil {
			rec["error"] = err.Error()
			errs = append(errs, fmt.Errorf("%v: %w", scheme, err))
		}
		out.line(rec)
		if err == nil {
			res.join, res.joinScheme = status, scheme
			return base, nil
		}
	}
	return "", fmt.Errorf("no signaling answer: %w", errors.Join(errs...))
}

// getJoin performs one GET and fills rec with the answer.
func getJoin(ctx context.Context, client *http.Client, base string, rec map[string]any) (map[string]any, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+"/v1/join", nil)
	if err != nil {
		return nil, err
	}
	// The User-Agent go-nethernet's endpoint client sends.
	req.Header.Set("User-Agent", "libhttpclient/1.0.0.0")
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, maxJoinBody))
	rec["status"] = resp.StatusCode
	rec["headers"] = resp.Header
	rec["body"] = string(body)
	if err != nil {
		return nil, fmt.Errorf("read body: %w", err)
	}
	if resp.StatusCode/100 != 2 {
		return nil, fmt.Errorf("status %v", resp.Status)
	}
	var status map[string]any
	if err := json.Unmarshal(body, &status); err != nil {
		return nil, fmt.Errorf("body is not a JSON object: %w", err)
	}
	rec["json"] = status
	return status, nil
}

// clientIdentity is the offer's a=identity: the test issuer's token for the online scenario (the token
// the Login carries), otherwise a self-signed ES384 token whose cpk is the base64 SPKI of the key, made
// by go-nethernet's GenerateServerIdentity (which builds exactly that token, with exp and iat). The
// bind-mismatch scenario signs it with a second key, so the offer and the Login prove different keys.
func clientIdentity(c config, key *ecdsa.PrivateKey, token string) (*nethernet.Identity, error) {
	switch c.scenario {
	case "online":
		return &nethernet.Identity{PrivateKey: key, Token: token, Domain: c.iss}, nil
	case "bind-mismatch":
		other, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
		if err != nil {
			return nil, err
		}
		return nethernet.GenerateServerIdentity(other, "self")
	}
	return nethernet.GenerateServerIdentity(key, "self")
}

// capture wraps a Signaling to keep the offer the Dialer sends and the answer it is notified of.
type capture struct {
	nethernet.Signaling
	mu            sync.Mutex
	offer, answer string
}

func (s *capture) Signal(ctx context.Context, signal *nethernet.Signal) error {
	if signal.Type == nethernet.SignalTypeOffer {
		s.mu.Lock()
		s.offer = signal.Data
		s.mu.Unlock()
	}
	return s.Signaling.Signal(ctx, signal)
}

func (s *capture) Notify(n nethernet.Notifier) func() {
	return s.Signaling.Notify(captureNotifier{s, n})
}

type captureNotifier struct {
	c *capture
	n nethernet.Notifier
}

func (n captureNotifier) NotifySignal(signal *nethernet.Signal) bool {
	if signal.Type == nethernet.SignalTypeAnswer {
		n.c.mu.Lock()
		n.c.answer = signal.Data
		n.c.mu.Unlock()
	}
	return n.n.NotifySignal(signal)
}

// record writes the captured offer and answer, and the answer's identity, and returns the answer's
// a=max-message-size value.
func (s *capture) record(out *output) string {
	s.mu.Lock()
	offer, answer := s.offer, s.answer
	s.mu.Unlock()
	for _, d := range []struct{ role, text string }{{"offer", offer}, {"answer", answer}} {
		if d.text != "" {
			out.line(map[string]any{"type": "sdp", "role": d.role, "text": d.text, "structure": sdpStructure(d.text)})
		}
	}
	if answer == "" {
		return ""
	}
	out.line(identityRecord(answer))
	for _, line := range sdpLines(answer) {
		if v, ok := strings.CutPrefix(line, "a=max-message-size:"); ok {
			return v
		}
	}
	return ""
}

// sdpLines splits an SDP into its lines without line ends.
func sdpLines(text string) []string {
	var lines []string
	for line := range strings.SplitSeq(text, "\n") {
		if line = strings.TrimSuffix(line, "\r"); line != "" {
			lines = append(lines, line)
		}
	}
	return lines
}

// sdpStructure returns each line's key without its value: the type for most lines ("v", "o"), the
// attribute name for a= lines ("a=identity") and the media type for m= lines ("m=application").
func sdpStructure(text string) []string {
	lines := sdpLines(text)
	keys := make([]string, 0, len(lines))
	for _, line := range lines {
		kind, value, _ := strings.Cut(line, "=")
		switch kind {
		case "a":
			name, _, _ := strings.Cut(value, ":")
			keys = append(keys, "a="+name)
		case "m":
			media, _, _ := strings.Cut(value, " ")
			keys = append(keys, "m="+media)
		default:
			keys = append(keys, kind)
		}
	}
	return keys
}

// identityRecord decodes the answer's a=identity: base64 JSON {"assertion": "<JSON string>", "idp": {..}}
// whose assertion holds the server token and the detached JWS over the fingerprints.
func identityRecord(answer string) map[string]any {
	rec := map[string]any{"type": "identity"}
	lines := sdpLines(answer)
	at, media := -1, -1
	for i, line := range lines {
		if at < 0 && strings.HasPrefix(line, "a=identity:") {
			at = i
		}
		if media < 0 && strings.HasPrefix(line, "m=") {
			media = i
		}
	}
	if at < 0 {
		rec["error"] = "the answer has no a=identity"
		return rec
	}
	rec["line"] = at
	rec["session_level"] = media < 0 || at < media
	value := strings.TrimPrefix(lines[at], "a=identity:")
	raw, err := base64.StdEncoding.DecodeString(value)
	if err != nil {
		if raw, err = base64.RawStdEncoding.DecodeString(value); err != nil {
			rec["error"] = "a=identity is not base64: " + err.Error()
			return rec
		}
		rec["unpadded"] = true
	}
	rec["json"] = string(raw)
	var outer struct {
		Assertion string         `json:"assertion"`
		IDP       map[string]any `json:"idp"`
	}
	if err := json.Unmarshal(raw, &outer); err != nil {
		rec["error"] = "a=identity is not the expected JSON: " + err.Error()
		return rec
	}
	rec["idp"] = outer.IDP
	var assertion struct {
		Fingerprints string `json:"fingerprints"`
		Token        string `json:"token"`
	}
	if err := json.Unmarshal([]byte(outer.Assertion), &assertion); err != nil {
		rec["error"] = "the assertion is not the expected JSON: " + err.Error()
		return rec
	}
	rec["token"] = assertion.Token
	rec["fingerprints"] = assertion.Fingerprints
	err = errors.Join(
		jwsPart(rec, "token_header", assertion.Token, 0),
		jwsPart(rec, "token_payload", assertion.Token, 1),
		jwsPart(rec, "fingerprints_header", assertion.Fingerprints, 0),
	)
	if err != nil {
		rec["error"] = err.Error()
	}
	return rec
}

// jwsPart decodes part i of a compact JWS into rec[key] (its JSON value) and rec[key+"_raw"] (the
// decoded text, exactly as signed).
func jwsPart(rec map[string]any, key, jws string, i int) error {
	parts := strings.Split(jws, ".")
	if len(parts) != 3 {
		return fmt.Errorf("%v: a JWS has 3 parts, got %v", key, len(parts))
	}
	b, err := base64.RawURLEncoding.DecodeString(parts[i])
	if err != nil {
		return fmt.Errorf("%v: %w", key, err)
	}
	rec[key+"_raw"] = string(b)
	var v any
	if err := json.Unmarshal(b, &v); err != nil {
		return fmt.Errorf("%v: %w", key, err)
	}
	rec[key] = v
	return nil
}
