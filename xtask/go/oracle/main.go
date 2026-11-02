// Command oracle is the gophertunnel test oracle for Anchor's Rust packet codecs and batch compression.
//
// All commands read and write record files: a sequence of [u32 LE id][u32 LE len][len bytes].
//
//	corpus -seed <u64> -samples <K> -limits <file> -out <file>
//	                                                 random packet payloads within Anchor's field limits,
//	                                                 id = packet ID
//	check -in <file> -out <file>                     gophertunnel decode + re-encode, id 0 ok / 1 error
//	compress -algo zlib|snappy -in <file> -out <file>
//	decompress -algo zlib|snappy -in <file> -out <file>
//
// The process exits non-zero only on oracle failures (bad arguments, I/O). Per-record failures are output
// records with id 1 and the error message as data.
package main

import (
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"os"

	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// ShieldID is the shield item network ID used for every Reader and Writer. Item user data carries an
// extra blocking tick when the item is the shield, so both sides of a comparison must agree on it; the
// Rust tests use the same value.
const ShieldID int32 = 358

// decompressLimit bounds decompressed output.
const decompressLimit = 64 << 20

type record struct {
	id   uint32
	data []byte
}

func failure(err error) record {
	return record{id: 1, data: []byte(err.Error())}
}

func readRecords(path string) ([]record, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out []record
	for len(b) > 0 {
		if len(b) < 8 {
			return nil, errors.New("truncated record header")
		}
		id, n := binary.LittleEndian.Uint32(b), binary.LittleEndian.Uint32(b[4:])
		b = b[8:]
		if uint64(n) > uint64(len(b)) {
			return nil, errors.New("truncated record data")
		}
		out = append(out, record{id: id, data: b[:n]})
		b = b[n:]
	}
	return out, nil
}

func writeRecords(path string, recs []record) error {
	var b []byte
	for _, r := range recs {
		b = binary.LittleEndian.AppendUint32(b, r.id)
		b = binary.LittleEndian.AppendUint32(b, uint32(len(r.data)))
		b = append(b, r.data...)
	}
	return os.WriteFile(path, b, 0o644)
}

func compression(algo string) (packet.Compression, error) {
	switch algo {
	case "zlib":
		return packet.FlateCompression, nil
	case "snappy":
		return packet.SnappyCompression, nil
	}
	return nil, fmt.Errorf("unknown -algo %q (want zlib or snappy)", algo)
}

// transform applies f to every record; errors become id 1 records.
func transform(in []record, f func([]byte) ([]byte, error)) []record {
	out := make([]record, 0, len(in))
	for _, r := range in {
		var b []byte
		err := try(func() {
			var err error
			b, err = f(r.data)
			if err != nil {
				panic(err)
			}
		})
		if err != nil {
			out = append(out, failure(err))
			continue
		}
		out = append(out, record{id: 0, data: b})
	}
	return out
}

func run(args []string) error {
	if len(args) == 0 {
		return errors.New("usage: oracle corpus|check|compress|decompress [flags]")
	}
	fs := flag.NewFlagSet(args[0], flag.ContinueOnError)
	in := fs.String("in", "", "input record file")
	out := fs.String("out", "", "output record file")
	var seed *uint64
	var samples *int
	var verbose *bool
	var limitsPath *string
	var algo *string
	switch args[0] {
	case "corpus":
		seed = fs.Uint64("seed", 0, "PCG seed")
		samples = fs.Int("samples", 64, "samples per packet")
		verbose = fs.Bool("v", false, "print per-packet attempt counts")
		limitsPath = fs.String("limits", "", "Anchor's field limits (protocol_limits.json)")
	case "compress", "decompress":
		algo = fs.String("algo", "", "zlib or snappy")
	case "check":
	default:
		return fmt.Errorf("unknown command %q", args[0])
	}
	if err := fs.Parse(args[1:]); err != nil {
		return err
	}
	if fs.NArg() != 0 {
		return fmt.Errorf("unexpected arguments %v", fs.Args())
	}
	if *out == "" {
		return errors.New("-out is required")
	}
	if args[0] == "corpus" {
		if *samples < 0 {
			return errors.New("-samples must not be negative")
		}
		if *limitsPath == "" {
			return errors.New("-limits is required")
		}
		lim, err := loadLimits(*limitsPath)
		if err != nil {
			return err
		}
		return writeRecords(*out, corpus(*seed, *samples, *verbose, lim))
	}
	if *in == "" {
		return errors.New("-in is required")
	}
	recs, err := readRecords(*in)
	if err != nil {
		return err
	}
	switch args[0] {
	case "check":
		return writeRecords(*out, check(recs))
	case "compress":
		c, err := compression(*algo)
		if err != nil {
			return err
		}
		return writeRecords(*out, transform(recs, c.Compress))
	default:
		c, err := compression(*algo)
		if err != nil {
			return err
		}
		return writeRecords(*out, transform(recs, func(b []byte) ([]byte, error) {
			return c.Decompress(b, decompressLimit)
		}))
	}
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "oracle:", err)
		os.Exit(2)
	}
}
