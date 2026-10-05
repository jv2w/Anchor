// Command leveldbdump prints every live key of a Bedrock world's LevelDB `db` folder with the
// SHA-256 of its value, in key order, one `hex(key) hex(sha256(value))` line each. Anchor's
// reader must print the same (`cargo test -p xtask --test import_reader`). It reads with
// df-mc/goleveldb, the reader dragonfly uses, on a copy of the folder: goleveldb replays the
// logs into new files when it opens a database, and the source must stay untouched.
package main

import (
	"bufio"
	"crypto/sha256"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"github.com/df-mc/goleveldb/leveldb"
	"github.com/df-mc/goleveldb/leveldb/opt"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: leveldbdump <world>/db")
		os.Exit(2)
	}
	if err := dump(os.Args[1], os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, "leveldbdump:", err)
		os.Exit(1)
	}
}

// dump writes the lines for the database in folder src to out.
func dump(src string, out io.Writer) error {
	tmp, err := os.MkdirTemp("", "leveldbdump")
	if err != nil {
		return err
	}
	defer os.RemoveAll(tmp)
	if err := copyFolder(src, tmp); err != nil {
		return err
	}
	db, err := leveldb.OpenFile(tmp, &opt.Options{ErrorIfMissing: true, Strict: opt.StrictAll})
	if err != nil {
		return err
	}
	defer db.Close()
	w := bufio.NewWriter(out)
	iter := db.NewIterator(nil, nil)
	for iter.Next() {
		sum := sha256.Sum256(iter.Value())
		fmt.Fprintf(w, "%x %x\n", iter.Key(), sum)
	}
	iter.Release()
	if err := iter.Error(); err != nil {
		return err
	}
	return w.Flush()
}

// copyFolder copies the regular files of src into dst.
func copyFolder(src, dst string) error {
	entries, err := os.ReadDir(src)
	if err != nil {
		return err
	}
	for _, e := range entries {
		if !e.Type().IsRegular() {
			continue
		}
		data, err := os.ReadFile(filepath.Join(src, e.Name()))
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(dst, e.Name()), data, 0o600); err != nil {
			return err
		}
	}
	return nil
}
