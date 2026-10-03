package main

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
)

// itemBlock is one entry of item_blocks.json: the block state the vanilla creative inventory gives a
// block item, which is the state BDS sends with that item.
type itemBlock struct {
	Item  string         `json:"item"`
	Name  string         `json:"name"`
	State map[string]any `json:"states"`
}

// itemBlockList reads the block states of the creative items of CloudburstMC's creative_items.json and
// checks them against dragonfly's creative_items.nbt (same properties per item name) and against the
// reference block list (same network ID). It returns the items whose creative entries all have one
// state, sorted by item name; items with several states (one name for several blocks) are left out.
func itemBlockList(dragonfly, cloudburst string, blocks []block) ([]itemBlock, error) {
	network := map[string]uint32{}
	for _, b := range blocks {
		network[b.Name+canonical(b.States)] = b.NetworkID
	}

	js, err := os.ReadFile(filepath.Join(cloudburst, "creative_items.json"))
	if err != nil {
		return nil, err
	}
	var cb struct {
		Items []struct {
			ID         string `json:"id"`
			BlockState string `json:"block_state_b64"`
		} `json:"items"`
	}
	if err := json.Unmarshal(js, &cb); err != nil {
		return nil, fmt.Errorf("creative_items.json: %w", err)
	}
	states := map[string]map[string]itemBlock{}
	for _, it := range cb.Items {
		if it.BlockState == "" {
			continue
		}
		raw, err := base64.StdEncoding.DecodeString(it.BlockState)
		if err != nil {
			return nil, fmt.Errorf("creative item %v: %w", it.ID, err)
		}
		r := &reader{b: raw}
		root, err := r.root()
		if err != nil {
			return nil, fmt.Errorf("creative item %v: %w", it.ID, err)
		}
		s, err := stateOf(root)
		if err != nil {
			return nil, fmt.Errorf("creative item %v: %w", it.ID, err)
		}
		id, ok := field[int32](root, "network_id", "int")
		key := s.Name + canonical(s.States)
		if want, known := network[key]; !ok || !known || uint32(id) != want {
			return nil, fmt.Errorf("creative item %v: state %v %v is not the reference state", it.ID, s.Name, canonical(s.States))
		}
		if states[it.ID] == nil {
			states[it.ID] = map[string]itemBlock{}
		}
		states[it.ID][key] = itemBlock{Item: it.ID, Name: s.Name, State: s.States}
	}

	raw, err := os.ReadFile(filepath.Join(dragonfly, "server", "item", "creative", "creative_items.nbt"))
	if err != nil {
		return nil, err
	}
	r := &reader{b: raw, network: true}
	root, err := r.root()
	if err != nil {
		return nil, fmt.Errorf("creative_items.nbt: %w", err)
	}
	list, ok := field[map[string]any](root, "items", "list")
	if !ok {
		return nil, errors.New("creative_items.nbt: no items list")
	}
	values, _ := list["values"].([]any)
	seen := map[string]map[string]bool{}
	for i, v := range values {
		c, ok := v.(map[string]any)["compound"].(map[string]any)
		if !ok {
			return nil, fmt.Errorf("creative_items.nbt: item %d is not a compound", i)
		}
		name, ok := field[string](c, "name", "string")
		if !ok {
			return nil, fmt.Errorf("creative_items.nbt: item %d has no name", i)
		}
		props, _ := field[map[string]any](c, "block_properties", "compound")
		if props == nil {
			props = map[string]any{}
		}
		if seen[name] == nil {
			seen[name] = map[string]bool{}
		}
		seen[name][canonical(props)] = true
	}
	// Every CloudburstMC state's properties must be among dragonfly's for the item (dragonfly omits
	// empty properties; both read as an empty compound).
	for item, byKey := range states {
		for _, s := range byKey {
			props := canonical(s.State)
			if !seen[item][props] {
				return nil, fmt.Errorf("creative item %v: dragonfly has no entry with properties %v", item, props)
			}
		}
	}

	var out []itemBlock
	for _, byKey := range states {
		if len(byKey) != 1 {
			continue
		}
		for _, s := range byKey {
			out = append(out, s)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Item < out[j].Item })
	return out, nil
}
