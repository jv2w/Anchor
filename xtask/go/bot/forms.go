package main

import (
	"encoding/json"
	"fmt"
	"sort"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// displays follows the forms, scoreboards and boss bar a server shows the bot, for scenarios that answer
// forms and read what the player sees. A session feeds it every packet with observe.
type displays struct {
	// forms holds the form requests not answered yet, in arrival order.
	forms []formRequest
	// objectives holds each objective by name; slots names the objective each display slot shows.
	objectives map[string]*objective
	slots      map[string]string
	// bar is the boss bar shown on the bot's own entity, nil when hidden.
	bar *bossBar
	// self is the bot's entity unique ID.
	self int64
}

// formRequest is one ModalFormRequest: its ID, its type ("modal", "form" or "custom_form"), its JSON and
// the JSON text as sent.
type formRequest struct {
	id   uint32
	kind string
	data map[string]any
	raw  []byte
}

// objective is one scoreboard objective and its lines by entry ID.
type objective struct {
	title string
	sort  int32
	lines map[int64]scoreLine
}

// scoreLine is one scoreboard entry: the fake player's name, or the entity of a player entry.
type scoreLine struct {
	name   string
	entity int64
	score  int32
}

// bossBar is the boss bar the bot sees.
type bossBar struct {
	title    string
	progress float32
	colour   uint8
}

func newDisplays(self int64) *displays {
	return &displays{objectives: map[string]*objective{}, slots: map[string]string{}, self: self}
}

// observe applies one packet of the server; packets of other kinds are ignored.
func (d *displays) observe(pk packet.Packet) error {
	switch pk := pk.(type) {
	case *packet.ModalFormRequest:
		var data map[string]any
		if err := json.Unmarshal(pk.FormData, &data); err != nil {
			return fmt.Errorf("form %v: %w", pk.FormID, err)
		}
		kind, _ := data["type"].(string)
		d.forms = append(d.forms, formRequest{id: pk.FormID, kind: kind, data: data, raw: pk.FormData})
	case *packet.ClientBoundCloseForm:
		d.forms = nil
	case *packet.SetDisplayObjective:
		d.objectives[pk.ObjectiveName] = &objective{title: pk.DisplayName, sort: pk.SortOrder, lines: map[int64]scoreLine{}}
		d.slots[pk.DisplaySlot] = pk.ObjectiveName
	case *packet.RemoveObjective:
		delete(d.objectives, pk.ObjectiveName)
		for slot, name := range d.slots {
			if name == pk.ObjectiveName {
				delete(d.slots, slot)
			}
		}
	case *packet.SetScore:
		for _, e := range pk.Entries {
			o, ok := d.objectives[e.ObjectiveName]
			if !ok {
				return fmt.Errorf("score for unknown objective %q", e.ObjectiveName)
			}
			switch e.IdentityType {
			case protocol.ScoreboardIdentityRemove:
				delete(o.lines, e.EntryID)
			case protocol.ScoreboardIdentityFakePlayer:
				o.lines[e.EntryID] = scoreLine{name: e.DisplayName, score: e.Score}
			default:
				o.lines[e.EntryID] = scoreLine{entity: e.EntityUniqueID, score: e.Score}
			}
		}
	case *packet.BossEvent:
		if pk.BossEntityUniqueID != d.self {
			return nil
		}
		switch pk.EventType {
		case packet.BossEventShow:
			d.bar = &bossBar{title: pk.BossBarTitle, progress: pk.HealthPercentage, colour: pk.Colour}
		case packet.BossEventHide:
			d.bar = nil
		case packet.BossEventTitle:
			if d.bar != nil {
				d.bar.title = pk.BossBarTitle
			}
		case packet.BossEventHealthPercentage:
			if d.bar != nil {
				d.bar.progress = pk.HealthPercentage
			}
		}
	}
	return nil
}

// lines returns the lines of the scoreboard in slot ("sidebar", "list" or "belowname") as the client
// orders them: by score in the objective's order, then by entry ID. Player entries show as "@<unique ID>".
func (d *displays) lines(slot string) (title string, lines []string, ok bool) {
	o, ok := d.objectives[d.slots[slot]]
	if !ok {
		return "", nil, false
	}
	type entry struct {
		id int64
		scoreLine
	}
	entries := make([]entry, 0, len(o.lines))
	for id, l := range o.lines {
		entries = append(entries, entry{id, l})
	}
	sort.Slice(entries, func(i, j int) bool {
		a, b := entries[i], entries[j]
		if a.score != b.score {
			return (a.score < b.score) == (o.sort == packet.ScoreboardSortOrderAscending)
		}
		return a.id < b.id
	})
	for _, e := range entries {
		if e.name != "" {
			lines = append(lines, e.name)
		} else {
			lines = append(lines, fmt.Sprintf("@%v", e.entity))
		}
	}
	return o.title, lines, true
}

// answer takes the oldest form waiting and returns the response choosing value: a bool for a modal, a
// button index for a menu, one value per element for a custom form, or nil to close it.
func (d *displays) answer(value any) (*packet.ModalFormResponse, formRequest, error) {
	if len(d.forms) == 0 {
		return nil, formRequest{}, fmt.Errorf("no form to answer")
	}
	f := d.forms[0]
	d.forms = d.forms[1:]
	if value == nil {
		return &packet.ModalFormResponse{
			FormID:       f.id,
			CancelReason: protocol.Option[uint8](packet.ModalFormCancelReasonUserClosed),
		}, f, nil
	}
	data, err := json.Marshal(value)
	if err != nil {
		return nil, f, err
	}
	return &packet.ModalFormResponse{FormID: f.id, ResponseData: protocol.Option(data)}, f, nil
}
