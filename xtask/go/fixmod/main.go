// Command fixmod writes the corrected gophertunnel module of package fix, so the oracle and the bot are
// built from the same source the extractor reads.
//
// Usage: go run ./fixmod -out <dir> (from the xtask/go module directory); then go build
// -modfile <dir>/go.mod.
package main

import (
	"flag"
	"fmt"
	"os"

	"anchor/xtask/go/fix"
)

func main() {
	out := flag.String("out", "", "directory for the corrected module and its go.mod")
	flag.Parse()
	if *out == "" {
		fmt.Fprintln(os.Stderr, "fixmod: -out is required")
		os.Exit(2)
	}
	if _, err := fix.Module(".", *out); err != nil {
		fmt.Fprintln(os.Stderr, "fixmod:", err)
		os.Exit(1)
	}
}
