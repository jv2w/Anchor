package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"net"
	"net/netip"
	"strconv"
	"strings"
	"time"
)

// RakNet offline message IDs and the magic, as in go-raknet's internal/message package.
const (
	idOpenConnectionRequest1      = 0x05
	idOpenConnectionReply1        = 0x06
	idOpenConnectionRequest2      = 0x07
	idOpenConnectionReply2        = 0x08
	idIncompatibleProtocolVersion = 0x19
	raknetProtocol                = 11
	probeMTU                      = 1492
)

var raknetMagic = []byte{0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78}

// probeRakNet runs the open connection handshake by hand, the way go-raknet's Dialer starts it, to record
// the MTU and the security flag the server answers with, and sends a request with protocol 10 to record the
// protocol version the server reports as its own. The half-open connection expires on the server.
func probeRakNet(ctx context.Context, addr string) map[string]any {
	rec := map[string]any{"type": "raknet", "client_protocol": raknetProtocol, "request_mtu": probeMTU}
	conn, err := (&net.Dialer{}).DialContext(ctx, "udp", addr)
	if err != nil {
		rec["error"] = err.Error()
		return rec
	}
	defer conn.Close()
	ocr1 := func(protocol byte) []byte {
		b := make([]byte, probeMTU-20-8)
		b[0] = idOpenConnectionRequest1
		copy(b[1:], raknetMagic)
		b[17] = protocol
		return b
	}
	reply, err := exchange(conn, ocr1(raknetProtocol), idOpenConnectionReply1)
	if err != nil {
		rec["error"] = "open connection request 1: " + err.Error()
		return rec
	}
	// Reply 1: ID, magic, server GUID (8), security (1), [cookie (4)], MTU (2).
	if len(reply) < 28 {
		rec["error"] = "short open connection reply 1"
		return rec
	}
	security := reply[25] != 0
	off := 26
	var cookie []byte
	if security {
		cookie = reply[26:30]
		off = 30
	}
	if len(reply) < off+2 {
		rec["error"] = "short open connection reply 1"
		return rec
	}
	rec["security"] = security
	mtu := binary.BigEndian.Uint16(reply[off:])
	rec["reply1_mtu"] = mtu

	// Request 2: ID, magic, [cookie (4), challenge flag (1)], server address, MTU (2), client GUID (8).
	// Like go-raknet, it carries the MTU of reply 1.
	raddr, err := netip.ParseAddrPort(conn.RemoteAddr().String())
	if err != nil {
		rec["error"] = err.Error()
		return rec
	}
	req2 := append([]byte{idOpenConnectionRequest2}, raknetMagic...)
	if security {
		req2 = append(append(req2, cookie...), 0)
	}
	ip := raddr.Addr().Unmap()
	if !ip.Is4() {
		rec["error"] = "the probe supports IPv4 addresses only"
		return rec
	}
	ip4 := ip.As4()
	req2 = append(req2, 4, ^ip4[0], ^ip4[1], ^ip4[2], ^ip4[3])
	req2 = binary.BigEndian.AppendUint16(req2, raddr.Port())
	req2 = binary.BigEndian.AppendUint16(req2, mtu)
	req2 = binary.BigEndian.AppendUint64(req2, uint64(time.Now().UnixNano()))
	reply, err = exchange(conn, req2, idOpenConnectionReply2)
	if err != nil {
		rec["error"] = "open connection request 2: " + err.Error()
		return rec
	}
	// Reply 2: ID, magic, server GUID (8), client address (7 for IPv4), MTU (2), security (1).
	if len(reply) < 25 {
		rec["error"] = "short open connection reply 2"
		return rec
	}
	addrLen := 7
	if reply[25] == 6 {
		addrLen = 29
	}
	if len(reply) < 25+addrLen+2 {
		rec["error"] = "short open connection reply 2"
		return rec
	}
	rec["reply2_mtu"] = binary.BigEndian.Uint16(reply[25+addrLen:])

	other, err := (&net.Dialer{}).DialContext(ctx, "udp", addr)
	if err != nil {
		rec["error"] = err.Error()
		return rec
	}
	defer other.Close()
	reply, err = exchange(other, ocr1(raknetProtocol-1), idIncompatibleProtocolVersion)
	if err != nil {
		rec["incompatible_error"] = err.Error()
	} else if len(reply) > 1 {
		rec["server_protocol"] = reply[1]
	}
	return rec
}

// exchange sends b up to three times and returns the first reply that starts with want.
func exchange(conn net.Conn, b []byte, want byte) ([]byte, error) {
	buf := make([]byte, 1500)
	for range 3 {
		if _, err := conn.Write(b); err != nil {
			return nil, err
		}
		_ = conn.SetReadDeadline(time.Now().Add(time.Second))
		for {
			n, err := conn.Read(buf)
			if err != nil {
				break
			}
			if n > 0 && buf[0] == want {
				return append([]byte(nil), buf[:n]...), nil
			}
		}
	}
	return nil, fmt.Errorf("no reply with ID %#x", want)
}

// pongRecord parses an unconnected pong: "MCPE;motd;protocol;version;players;max;guid;level;mode;...".
func pongRecord(pong []byte, err error) map[string]any {
	rec := map[string]any{"type": "pong"}
	if err != nil {
		rec["error"] = err.Error()
		return rec
	}
	rec["data"] = string(pong)
	parts := strings.Split(string(pong), ";")
	if len(parts) > 3 {
		if p, err := strconv.Atoi(parts[2]); err == nil {
			rec["protocol"] = p
		}
		rec["version"] = parts[3]
	}
	return rec
}
