package main

import (
	"bytes"
	"compress/flate"
	"crypto/aes"
	"crypto/cipher"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"reflect"
	"strings"

	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Recording caps: larger values are replaced by their length and SHA-256.
const (
	maxBytes   = 64
	maxString  = 4096
	maxElems   = 256
	maxDepth   = 64
	batchLimit = 812
)

// batchReader passes whole transport messages to gophertunnel's packet.Decoder and records raw facts
// about each batch on the way. It decrypts a copy with its own key stream, because the Decoder decrypts
// in place and does not expose the compression byte or the bytes after the final DEFLATE block. It
// passes the transport's batch header and encryption capability on to the Decoder.
type batchReader struct {
	conn         packetConn
	out          *output
	header       []byte
	noEncryption bool
	compressed   bool
	stream       cipher.Stream
	key          []byte
	counter      uint64
	batches      int
	readErr      error

	maxPackets, maxDecompressed, maxWire       int
	zlibTrailing, highHeaderBits, over812, bad int
	algorithms                                 map[int]int
}

// newBatchReader reads conn with gophertunnel's defaults for a transport without the optional
// capabilities: the 0xfe header and encryption.
func newBatchReader(conn packetConn, out *output) *batchReader {
	r := &batchReader{conn: conn, out: out, header: []byte{0xfe}}
	if h, ok := conn.(packet.BatchHeaderer); ok {
		r.header = h.BatchHeader()
	}
	if d, ok := conn.(packet.EncryptionDisabler); ok {
		r.noEncryption = d.DisableEncryption()
	}
	return r
}

// BatchHeader is the transport's batch header.
func (r *batchReader) BatchHeader() []byte {
	return r.header
}

// DisableEncryption reports whether the transport forbids Minecraft encryption.
func (r *batchReader) DisableEncryption() bool {
	return r.noEncryption
}

// Read is not used: the Decoder reads whole messages through ReadPacket.
func (r *batchReader) Read([]byte) (int, error) {
	return 0, errors.New("batchReader supports ReadPacket only")
}

// ReadPacket reads one transport message, records its batch facts and returns it unchanged.
func (r *batchReader) ReadPacket() ([]byte, error) {
	b, err := r.conn.ReadPacket()
	if err != nil {
		r.readErr = err
		return nil, err
	}
	if s, ok := r.conn.(*segmentConn); ok {
		s.flushRecords(r.out)
	}
	r.out.line(r.analyse(bytes.Clone(b)))
	r.batches++
	return b, nil
}

func (r *batchReader) enableEncryption(key [32]byte) {
	block, _ := aes.NewCipher(key[:])
	iv := append(bytes.Clone(key[:12]), 0, 0, 0, 2)
	r.stream = cipher.NewCTR(block, iv)
	r.key = bytes.Clone(key[:])
}

// analyse returns the batch record for one raw message (owned by the caller).
func (r *batchReader) analyse(b []byte) map[string]any {
	rec := map[string]any{"type": "batch", "index": r.batches, "wire_size": len(b), "encrypted": r.stream != nil}
	r.maxWire = max(r.maxWire, len(b))
	fail := func(err error) map[string]any {
		r.bad++
		rec["error"] = err.Error()
		return rec
	}
	if !bytes.HasPrefix(b, r.header) {
		return fail(fmt.Errorf("missing batch header %x", r.header))
	}
	data := b[len(r.header):]
	if r.stream != nil {
		r.stream.XORKeyStream(data, data)
		if len(data) < 8 {
			return fail(errors.New("encrypted batch shorter than its checksum"))
		}
		var counter [8]byte
		binary.LittleEndian.PutUint64(counter[:], r.counter)
		r.counter++
		h := sha256.New()
		h.Write(counter[:])
		h.Write(data[:len(data)-8])
		h.Write(r.key)
		if !bytes.Equal(h.Sum(nil)[:8], data[len(data)-8:]) {
			return fail(errors.New("checksum mismatch"))
		}
		data = data[:len(data)-8]
	}
	if r.compressed {
		if len(data) == 0 {
			return fail(errors.New("missing compression byte"))
		}
		id := int(data[0])
		rec["compression"] = id
		if r.algorithms == nil {
			r.algorithms = map[int]int{}
		}
		r.algorithms[id]++
		var err error
		switch id {
		case 0xff:
			data = data[1:]
		case 0:
			var trailing int
			data, trailing, err = inflate(data[1:])
			rec["zlib_trailing"] = trailing
			if trailing > 0 {
				r.zlibTrailing++
			}
		case 1:
			data, err = packet.SnappyCompression.Decompress(data[1:], maxDecompressed)
		default:
			err = fmt.Errorf("unknown compression id %v", id)
		}
		if err != nil {
			return fail(err)
		}
	}
	rec["decompressed_size"] = len(data)
	r.maxDecompressed = max(r.maxDecompressed, len(data))
	count, high, err := walkBatch(data)
	rec["packets"] = count
	rec["header_high_bits"] = high
	r.maxPackets = max(r.maxPackets, count)
	if high != 0 {
		r.highHeaderBits++
	}
	if count > batchLimit {
		r.over812++
	}
	if err != nil {
		return fail(err)
	}
	return rec
}

// inflate decompresses raw DEFLATE data and returns how many bytes follow the final block. A bytes.Reader
// is an io.ByteReader, so the flate reader never reads past the final block.
func inflate(b []byte) ([]byte, int, error) {
	src := bytes.NewReader(b)
	out, err := io.ReadAll(io.LimitReader(flate.NewReader(src), maxDecompressed+1))
	if err != nil {
		return nil, 0, fmt.Errorf("inflate: %w", err)
	}
	if len(out) > maxDecompressed {
		return nil, 0, errors.New("inflate: batch exceeds the size limit")
	}
	return out, src.Len(), nil
}

// walkBatch counts the packets of a decompressed batch and ORs together the header bits above bit 13.
func walkBatch(b []byte) (count int, high uint32, err error) {
	for len(b) > 0 {
		length, n := binary.Uvarint(b)
		if n <= 0 || length > uint64(len(b)-n) || length == 0 {
			return count, high, errors.New("malformed packet length")
		}
		pk := b[n : n+int(length)]
		b = b[n+int(length):]
		header, hn := binary.Uvarint(pk)
		if hn <= 0 || header > math.MaxUint32 {
			return count, high, errors.New("malformed packet header")
		}
		high |= uint32(header >> 14)
		count++
	}
	return count, high, nil
}

// summary adds the batch statistics to the result record.
func (r *batchReader) summary(m map[string]any) {
	algorithms := map[string]int{}
	for id, n := range r.algorithms {
		algorithms[fmt.Sprint(id)] = n
	}
	m["batches"] = r.batches
	m["batch_compression_ids"] = algorithms
	m["max_batch_packets"] = r.maxPackets
	m["max_batch_decompressed"] = r.maxDecompressed
	m["max_batch_wire"] = r.maxWire
	m["zlib_trailing_batches"] = r.zlibTrailing
	m["header_high_bit_batches"] = r.highHeaderBits
	m["batches_over_812"] = r.over812
	m["bad_batches"] = r.bad
}

// hashed replaces a large value by its length and the SHA-256 of its JSON form.
func hashed(n int, v any) map[string]any {
	b, _ := json.Marshal(v)
	sum := sha256.Sum256(b)
	return map[string]any{"len": n, "sha256": hex.EncodeToString(sum[:])}
}

var uuidType = reflect.TypeOf(uuid.UUID{})

// toJSON converts a gophertunnel value into a JSON tree. Structs become objects of their exported fields,
// Optional values become their value or null, byte data becomes base64 (or length and hash when large),
// and non-finite floats become strings.
func toJSON(v reflect.Value, depth int) any {
	if depth > maxDepth {
		return "<too deep>"
	}
	if v.IsValid() && v.Type() == uuidType {
		return v.Interface().(uuid.UUID).String()
	}
	switch v.Kind() {
	case reflect.Invalid:
		return nil
	case reflect.Pointer, reflect.Interface:
		if v.IsNil() {
			return nil
		}
		return toJSON(v.Elem(), depth+1)
	case reflect.Bool:
		return v.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return v.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return v.Uint()
	case reflect.Float32, reflect.Float64:
		f := v.Float()
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return fmt.Sprint(f)
		}
		if v.Kind() == reflect.Float32 {
			// Keep the float32 value's shortest form rather than its float64 expansion.
			return json.Number(fmt.Sprint(float32(f)))
		}
		return f
	case reflect.String:
		if v.Len() > maxString {
			return hashed(v.Len(), v.String())
		}
		return v.String()
	case reflect.Struct:
		if strings.HasPrefix(v.Type().Name(), "Optional[") {
			out := v.MethodByName("Value").Call(nil)
			if !out[1].Bool() {
				return nil
			}
			return toJSON(out[0], depth+1)
		}
		m := make(map[string]any, v.NumField())
		for i := range v.NumField() {
			if f := v.Type().Field(i); f.IsExported() {
				m[f.Name] = toJSON(v.Field(i), depth+1)
			}
		}
		return m
	case reflect.Slice, reflect.Array:
		if v.Kind() == reflect.Slice && v.IsNil() {
			return []any{}
		}
		if v.Type().Elem().Kind() == reflect.Uint8 {
			b := make([]byte, v.Len())
			reflect.Copy(reflect.ValueOf(b), v)
			if len(b) > maxBytes {
				sum := sha256.Sum256(b)
				return map[string]any{"len": len(b), "sha256": hex.EncodeToString(sum[:])}
			}
			return base64.StdEncoding.EncodeToString(b)
		}
		out := make([]any, v.Len())
		for i := range out {
			out[i] = toJSON(v.Index(i), depth+1)
		}
		if len(out) > maxElems {
			return hashed(len(out), out)
		}
		return out
	case reflect.Map:
		m := make(map[string]any, v.Len())
		iter := v.MapRange()
		for iter.Next() {
			m[fmt.Sprint(iter.Key().Interface())] = toJSON(iter.Value(), depth+1)
		}
		return m
	default:
		return fmt.Sprintf("<%v>", v.Kind())
	}
}
