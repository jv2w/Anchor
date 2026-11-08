package fix

import (
	"os/exec"
	"path/filepath"
	"testing"
)

func TestModuleGoModLoadsFromADirectoryWithASpace(t *testing.T) {
	modfile, err := Module("..", filepath.Join(t.TempDir(), "with space"))
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command("go", "list", "-modfile", modfile, "-m", modulePath)
	cmd.Dir = ".."
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("go list with the written go.mod: %v\n%s", err, out)
	}
}
