package main

import (
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"strings"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// limits holds Anchor's per-field decode limits (xtask/data/reference/protocol_limits.json): the most
// bytes of a string or elements of a list, keyed by "<Go type>.<field>", with "[]" appended for the
// elements of a list field. The corpus keeps only samples within them, because Anchor rejects the rest.
type limits map[string]int

// loadLimits reads a limits file.
func loadLimits(path string) (limits, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var l limits
	if err := json.Unmarshal(b, &l); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return l, nil
}

// optionalPrefix is the start of the reflect name of protocol.Optional[T].
const optionalPrefix = "Optional["

// violation returns the first field of v over its limit, or "" if none is.
func (l limits) violation(v reflect.Value) string {
	return l.walk(v, "")
}

// walk checks v, the value of the field with limit key key ("" for none), and everything inside it.
func (l limits) walk(v reflect.Value, key string) string {
	switch v.Kind() {
	case reflect.Pointer, reflect.Interface:
		if v.IsNil() {
			return ""
		}
		return l.walk(v.Elem(), key)
	case reflect.String:
		if max, ok := l[key]; ok && v.Len() > max {
			return fmt.Sprintf("%s: %d bytes > %d", key, v.Len(), max)
		}
	case reflect.Slice:
		if max, ok := l[key]; ok && v.Len() > max {
			return fmt.Sprintf("%s: %d elements > %d", key, v.Len(), max)
		}
		elem := ""
		if key != "" {
			elem = key + "[]"
		}
		for i := 0; i < v.Len(); i++ {
			if s := l.walk(v.Index(i), elem); s != "" {
				return s
			}
		}
	case reflect.Array:
		for i := 0; i < v.Len(); i++ {
			if s := l.walk(v.Index(i), ""); s != "" {
				return s
			}
		}
	case reflect.Struct:
		t := v.Type()
		if t.PkgPath() == protocolPkg && strings.HasPrefix(t.Name(), optionalPrefix) {
			// Optional[T]{set, val}: the value takes the field's limit.
			if v.Field(0).Bool() {
				return l.walk(v.Field(1), key)
			}
			return ""
		}
		for i := 0; i < t.NumField(); i++ {
			if !t.Field(i).IsExported() {
				continue
			}
			if s := l.walk(v.Field(i), t.String()+"."+t.Field(i).Name); s != "" {
				return s
			}
		}
	}
	return ""
}

// protocolPkg is the import path of gophertunnel's protocol package, which defines Optional.
var protocolPkg = reflect.TypeOf(protocol.Optional[int]{}).PkgPath()
