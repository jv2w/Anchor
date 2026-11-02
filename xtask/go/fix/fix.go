// Package fix corrects defects of the pinned gophertunnel source for every Go tool of xtask: it writes a
// corrected copy of the module and a go.mod that replaces gophertunnel with it, so the extractor, the
// oracle and the bot all build with -modfile from the same corrected source. Each defect, with its
// evidence, is listed under "gophertunnel defects" in .claude/docs/protocol.md.
//
// A correction replaces text that must occur exactly once in its file, so a gophertunnel update that
// moves or fixes the text fails here instead of passing silently.
package fix

import (
	"bytes"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	"golang.org/x/tools/go/packages"
)

const (
	modulePath  = "github.com/sandertv/gophertunnel"
	protocolPkg = modulePath + "/minecraft/protocol"
	// protocolDir is the directory of protocolPkg in the module.
	protocolDir = "minecraft/protocol"
)

// replacement replaces old with new in one file of the protocol package.
type replacement struct {
	file     string
	old, new string
}

var replacements = []replacement{
	// ClientboundUpdateSoundData: the event is a tagged variant, written as its uvarint32 variant
	// index followed by the event type.
	{
		file: "sound.go",
		old:  "func (x *SoundDataUpdate) Marshal(io IO) {\n\tio.Uint8(&x.Type)\n",
		new: "func (x *SoundDataUpdate) Marshal(io IO) {\n" +
			"\tvariant := uint32(x.Type)\n" +
			"\tio.Varuint32(&variant)\n" +
			"\tio.Uint8(&x.Type)\n" +
			"\tif variant != uint32(x.Type) {\n" +
			"\t\tio.InvalidValue(variant, \"sound data update variant\", \"differs from the type\")\n" +
			"\t}\n",
	},
	// PieceType: 1.26.60 added CoCo (28) before Unsupported (29).
	{
		file: "skin.go",
		old:  "\tPieceTypeEmote\n\tPieceTypeUnsupported\n",
		new:  "\tPieceTypeEmote\n\tPieceTypeCoCo\n\tPieceTypeUnsupported\n",
	},
	// Skin.validate: the size check multiplies in uint32, so a product past 2^32 wraps and a skin whose
	// dimensions disagree with its data passes. Compare the pixel count in uint64 instead.
	{
		file: "skin.go",
		old:  "\tif x.SkinImageHeight*x.SkinImageWidth*4 != uint32(len(x.SkinData)) {\n",
		new: "\tif uint64(x.SkinImageHeight)*uint64(x.SkinImageWidth) != uint64(len(x.SkinData))/4 ||\n" +
			"\t\tuint64(len(x.SkinData))/4*4 != uint64(len(x.SkinData)) {\n",
	},
	{
		file: "skin.go",
		old:  "\tif x.CapeImageHeight*x.CapeImageWidth*4 != uint32(len(x.CapeData)) {\n",
		new: "\tif uint64(x.CapeImageHeight)*uint64(x.CapeImageWidth) != uint64(len(x.CapeData))/4 ||\n" +
			"\t\tuint64(len(x.CapeData))/4*4 != uint64(len(x.CapeData)) {\n",
	},
	{
		file: "skin.go",
		old:  "\t\tif animation.ImageHeight*animation.ImageWidth*4 != uint32(len(animation.ImageData)) {\n",
		new: "\t\tif uint64(animation.ImageHeight)*uint64(animation.ImageWidth) != uint64(len(animation.ImageData))/4 ||\n" +
			"\t\t\tuint64(len(animation.ImageData))/4*4 != uint64(len(animation.ImageData)) {\n",
	},
	// Entity data keys: 139 is reserved and 140 is the name tag render distance. The arrow and firework
	// shooter keys are actor-specific meanings of 17 and 18. 141 is a byte the official server 1.26.60
	// sends players (P6), with no public name.
	{
		file: "entity_metadata.go",
		old: "\tEntityDataKeyArrowShooterID\n\tEntityDataKeyFireworkDirection\n\tEntityDataKeyFireworkShooterID\n" +
			"\tEntityDataKeyReserved139\n\tEntityDataKeyNameplateRenderDistanceMax\n)\n",
		new: "\tEntityDataKeyReserved139\n\tEntityDataKeyNameplateRenderDistanceMax\n\tEntityDataKeyReserved141\n)\n",
	},
	{
		file: "entity_metadata.go",
		old:  "\tEntityDataKeyUnknownHorseInteger25 = EntityDataKeyUsingItem\n)\n",
		new: "\tEntityDataKeyUnknownHorseInteger25 = EntityDataKeyUsingItem\n\n" +
			"\tEntityDataKeyArrowShooterID    = EntityDataKeyDisplayOffset\n" +
			"\tEntityDataKeyFireworkDirection = EntityDataKeyDisplayOffset\n" +
			"\tEntityDataKeyFireworkShooterID = EntityDataKeyCustomDisplay\n)\n",
	},
}

// Module writes the corrected module to out/gophertunnel and out/go.mod (with out/go.sum): the go.mod of
// the module in dir with gophertunnel replaced by the copy. It returns the path of that go.mod, for
// -modfile.
func Module(dir, out string) (string, error) {
	cfg := &packages.Config{Mode: packages.NeedName | packages.NeedModule, Dir: dir}
	pkgs, err := packages.Load(cfg, protocolPkg)
	if err != nil {
		return "", err
	}
	if len(pkgs) != 1 || pkgs[0].Module == nil || pkgs[0].Module.Replace != nil {
		return "", fmt.Errorf("fix: %s does not load from the module cache", protocolPkg)
	}
	copyDir, err := filepath.Abs(filepath.Join(out, "gophertunnel"))
	if err != nil {
		return "", err
	}
	if err := os.RemoveAll(copyDir); err != nil {
		return "", err
	}
	if err := copyTree(pkgs[0].Module.Dir, copyDir); err != nil {
		return "", err
	}
	for _, r := range replacements {
		path := filepath.Join(copyDir, protocolDir, r.file)
		src, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		if n := bytes.Count(src, []byte(r.old)); n != 1 {
			return "", fmt.Errorf("fix: %s: the text to replace occurs %d times, want 1: %q", r.file, n, r.old)
		}
		if err := os.WriteFile(path, bytes.Replace(src, []byte(r.old), []byte(r.new), 1), 0o644); err != nil {
			return "", err
		}
	}
	mod, err := os.ReadFile(filepath.Join(dir, "go.mod"))
	if err != nil {
		return "", err
	}
	mod = fmt.Appendf(mod, "\nreplace %s => %s\n", modulePath, filepath.ToSlash(copyDir))
	modfile := filepath.Join(out, "go.mod")
	if err := os.WriteFile(modfile, mod, 0o644); err != nil {
		return "", err
	}
	sum, err := os.ReadFile(filepath.Join(dir, "go.sum"))
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(filepath.Join(out, "go.sum"), sum, 0o644); err != nil {
		return "", err
	}
	return filepath.Abs(modfile)
}

// copyTree copies the regular files under src to dst, writable (module cache files are read-only).
func copyTree(src, dst string) error {
	return filepath.WalkDir(src, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		target := filepath.Join(dst, rel)
		if d.IsDir() {
			return os.MkdirAll(target, 0o755)
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		return os.WriteFile(target, data, 0o644)
	})
}
