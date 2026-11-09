package main

import (
	"context"
	"testing"
	"time"

	"github.com/sandertv/go-raknet"
)

func TestIdleEndsAtTheTimeLimit(t *testing.T) {
	// A listener that never closes the connection, unlike a server's login deadline.
	l, err := raknet.Listen("127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	held := make(chan struct{})
	defer close(held)
	go func() {
		if conn, err := l.Accept(); err == nil {
			defer conn.Close()
			<-held
		}
	}()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	done := make(chan *result, 1)
	go func() { done <- playLimit(ctx, config{addr: l.Addr().String(), scenario: "idle"}) }()
	select {
	case res := <-done:
		if res.pass {
			t.Fatalf("the idle scenario passed at its time limit: %v", res.reason)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the idle scenario ran past its time limit")
	}
}
