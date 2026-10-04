package main

import (
	"reflect"
	"testing"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func TestDisplaysFollowFormsScoreboardsAndTheBossBar(t *testing.T) {
	d := newDisplays(7)
	steps := []packet.Packet{
		&packet.ModalFormRequest{FormID: 3, FormData: []byte(`{"type":"form","title":"Lobby","elements":[]}`)},
		&packet.SetDisplayObjective{DisplaySlot: "sidebar", ObjectiveName: "anchor:sidebar", DisplayName: "Lobby"},
		&packet.SetScore{Entries: []protocol.ScoreboardEntry{
			{EntryID: -2, ObjectiveName: "anchor:sidebar", Score: 2, IdentityType: protocol.ScoreboardIdentityFakePlayer, DisplayName: "b"},
			{EntryID: -1, ObjectiveName: "anchor:sidebar", Score: 1, IdentityType: protocol.ScoreboardIdentityFakePlayer, DisplayName: "a"},
			{EntryID: 9, ObjectiveName: "anchor:sidebar", Score: 3, IdentityType: protocol.ScoreboardIdentityPlayer, EntityUniqueID: 9},
		}},
		&packet.SetScore{Entries: []protocol.ScoreboardEntry{
			{EntryID: -2, ObjectiveName: "anchor:sidebar", IdentityType: protocol.ScoreboardIdentityRemove},
		}},
		&packet.BossEvent{BossEntityUniqueID: 7, EventType: packet.BossEventShow, BossBarTitle: "x", HealthPercentage: 1},
		&packet.BossEvent{BossEntityUniqueID: 7, EventType: packet.BossEventHealthPercentage, BossBarTitle: "x", HealthPercentage: 0.5},
		&packet.BossEvent{BossEntityUniqueID: 8, EventType: packet.BossEventHide},
	}
	for _, pk := range steps {
		if err := d.observe(pk); err != nil {
			t.Fatal(err)
		}
	}
	title, lines, ok := d.lines("sidebar")
	if !ok || title != "Lobby" || !reflect.DeepEqual(lines, []string{"a", "@9"}) {
		t.Fatalf("sidebar %q %q %v", title, lines, ok)
	}
	if d.bar == nil || d.bar.title != "x" || d.bar.progress != 0.5 {
		t.Fatalf("bar %+v", d.bar)
	}
	resp, f, err := d.answer(1)
	if err != nil || f.kind != "form" || resp.FormID != 3 {
		t.Fatalf("answer %+v %+v %v", resp, f, err)
	}
	if data, ok := resp.ResponseData.Value(); !ok || string(data) != "1" {
		t.Fatalf("response data %q", data)
	}
	if _, _, err := d.answer(nil); err == nil {
		t.Fatal("answered a form that was not sent")
	}
	if err := d.observe(&packet.RemoveObjective{ObjectiveName: "anchor:sidebar"}); err != nil {
		t.Fatal(err)
	}
	if _, _, ok := d.lines("sidebar"); ok {
		t.Fatal("removed objective still shown")
	}
}
