package main

import (
	"context"
	"encoding/binary"
	"net"
	"testing"
	"time"
)

func TestProbeRejectsAReply2WithoutItsAddress(t *testing.T) {
	// A server whose reply 2 ends right after the server GUID, before the address version byte.
	server, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	go func() {
		buf := make([]byte, 1500)
		for {
			n, addr, err := server.ReadFrom(buf)
			if err != nil {
				return
			}
			var reply []byte
			switch {
			case n > 0 && buf[0] == idOpenConnectionRequest1:
				reply = append(append([]byte{idOpenConnectionReply1}, raknetMagic...), make([]byte, 8+1)...)
				reply = binary.BigEndian.AppendUint16(reply, probeMTU)
			case n > 0 && buf[0] == idOpenConnectionRequest2:
				reply = append(append([]byte{idOpenConnectionReply2}, raknetMagic...), make([]byte, 8)...)
			default:
				continue
			}
			_, _ = server.WriteTo(reply, addr)
		}
	}()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	rec := probeRakNet(ctx, server.LocalAddr().String())
	if rec["error"] != "short open connection reply 2" {
		t.Fatalf("probe record %v, want a short reply 2", rec)
	}
}
