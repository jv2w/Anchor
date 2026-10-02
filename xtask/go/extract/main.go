// Command extract writes the protocol IR (see ir.go) of the pinned gophertunnel protocol and packet packages.
//
// Usage: go run ./extract -out ../data/protocol.json (from the xtask/go module directory).
// It fails with file:line and the construct on anything it does not understand.
package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"go/ast"
	"go/constant"
	"go/token"
	"go/types"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"golang.org/x/tools/go/packages"
)

const (
	modulePath  = "github.com/sandertv/gophertunnel"
	protocolPkg = modulePath + "/minecraft/protocol"
	packetPkg   = protocolPkg + "/packet"
	// commit is the full hash of the pinned pseudo-version in go.mod; main checks the two agree.
	commit = "80c811b6186016b3860c358368cfa47e507f26e9"
)

// failure aborts the extraction; main reports it.
type failure struct{ msg string }

func main() {
	out := flag.String("out", "", "output path of protocol.json")
	flag.Parse()
	if *out == "" {
		fmt.Fprintln(os.Stderr, "extract: -out is required")
		os.Exit(2)
	}
	data, err := run()
	if err != nil {
		fmt.Fprintln(os.Stderr, "extract:", err)
		os.Exit(1)
	}
	if err := os.WriteFile(*out, data, 0o644); err != nil {
		fmt.Fprintln(os.Stderr, "extract:", err)
		os.Exit(1)
	}
}

func run() (data []byte, err error) {
	defer func() {
		if r := recover(); r != nil {
			f, ok := r.(failure)
			if !ok {
				panic(r)
			}
			err = fmt.Errorf("%s", f.msg)
		}
	}()
	cfg := &packages.Config{Mode: packages.NeedName | packages.NeedFiles | packages.NeedSyntax |
		packages.NeedTypes | packages.NeedTypesInfo | packages.NeedModule}
	pkgs, err := packages.Load(cfg, protocolPkg, packetPkg)
	if err != nil {
		return nil, err
	}
	if packages.PrintErrors(pkgs) > 0 {
		return nil, fmt.Errorf("package load failed")
	}
	ex := newExtractor(pkgs)
	ir := ex.extract()
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(ir); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// funcInfo is a function or method declaration of protocol or packet.
type funcInfo struct {
	decl *ast.FuncDecl
	pkg  *packages.Package
	obj  *types.Func
}

type extractor struct {
	fset     *token.FileSet
	pkgs     map[string]*packages.Package
	ioType   *types.Named
	optional *types.Named
	funcs    map[*types.Func]*funcInfo
	typeDocs map[*types.TypeName]*ast.CommentGroup
	fieldDoc map[*types.Var]*ast.CommentGroup

	types     map[string]*TypeDef
	typeQueue []*types.Named
	seenTypes map[*types.Named]bool
	fnSeen    map[*types.Func]bool
	fnQueue   []*types.Func
	functions map[string]Function
	enums     map[string]Enum
	unions    map[string]Union
	consts    map[string]Const
}

func newExtractor(pkgs []*packages.Package) *extractor {
	ex := &extractor{
		pkgs:      map[string]*packages.Package{},
		funcs:     map[*types.Func]*funcInfo{},
		typeDocs:  map[*types.TypeName]*ast.CommentGroup{},
		fieldDoc:  map[*types.Var]*ast.CommentGroup{},
		types:     map[string]*TypeDef{},
		seenTypes: map[*types.Named]bool{},
		fnSeen:    map[*types.Func]bool{},
		functions: map[string]Function{},
		enums:     map[string]Enum{},
		unions:    map[string]Union{},
		consts:    map[string]Const{},
	}
	for _, p := range pkgs {
		ex.fset = p.Fset
		ex.pkgs[p.PkgPath] = p
		for _, f := range p.Syntax {
			for _, d := range f.Decls {
				switch d := d.(type) {
				case *ast.FuncDecl:
					obj := p.TypesInfo.Defs[d.Name].(*types.Func)
					ex.funcs[obj] = &funcInfo{decl: d, pkg: p, obj: obj}
				case *ast.GenDecl:
					ex.indexTypeDecl(p, d)
				}
			}
		}
	}
	scope := ex.pkgs[protocolPkg].Types.Scope()
	ex.ioType = scope.Lookup("IO").Type().(*types.Named)
	ex.optional = scope.Lookup("Optional").Type().(*types.Named)
	return ex
}

func (ex *extractor) indexTypeDecl(p *packages.Package, d *ast.GenDecl) {
	if d.Tok != token.TYPE {
		return
	}
	for _, s := range d.Specs {
		ts := s.(*ast.TypeSpec)
		doc := ts.Doc
		if doc == nil && len(d.Specs) == 1 {
			doc = d.Doc
		}
		obj := p.TypesInfo.Defs[ts.Name].(*types.TypeName)
		ex.typeDocs[obj] = doc
		if st, ok := ts.Type.(*ast.StructType); ok {
			for _, f := range st.Fields.List {
				for _, n := range f.Names {
					ex.fieldDoc[p.TypesInfo.Defs[n].(*types.Var)] = f.Doc
				}
			}
		}
	}
}

func (ex *extractor) fail(pos token.Pos, format string, args ...any) {
	p := ex.fset.Position(pos)
	file := p.Filename
	if i := strings.Index(filepath.ToSlash(file), "/minecraft/"); i >= 0 {
		file = filepath.ToSlash(file)[i+1:]
	}
	panic(failure{fmt.Sprintf("%s:%d: %s", file, p.Line, fmt.Sprintf(format, args...))})
}

func (ex *extractor) extract() *IR {
	ir := &IR{Source: ex.source()}
	ir.IOMethods = ex.ioMethods()
	ir.Packets = ex.packets()
	for _, pk := range ir.Packets {
		ex.needType(ex.lookupNamed(pk.Type))
	}
	// The union lookup functions and the stack request action variant mapping are only called by the
	// hand-written IO methods (reader.go, writer.go); seed them.
	for obj, fi := range ex.funcs {
		name := obj.Name()
		if fi.decl.Recv == nil && (strings.HasPrefix(name, "lookup") || name == "stackRequestActionVariant") {
			ex.needFunc(obj)
		}
	}
	for len(ex.typeQueue) > 0 || len(ex.fnQueue) > 0 {
		if n := len(ex.typeQueue); n > 0 {
			t := ex.typeQueue[n-1]
			ex.typeQueue = ex.typeQueue[:n-1]
			ex.emitType(t)
			continue
		}
		n := len(ex.fnQueue)
		f := ex.fnQueue[n-1]
		ex.fnQueue = ex.fnQueue[:n-1]
		ex.emitFunc(f)
	}
	for _, t := range sortedKeys(ex.types) {
		ir.Types = append(ir.Types, *ex.types[t])
	}
	for _, k := range sortedKeys(ex.functions) {
		ir.Functions = append(ir.Functions, ex.functions[k])
	}
	for _, k := range sortedKeys(ex.enums) {
		ir.Enums = append(ir.Enums, ex.enums[k])
	}
	for _, k := range sortedKeys(ex.unions) {
		ir.Unions = append(ir.Unions, ex.unions[k])
	}
	for _, k := range sortedKeys(ex.consts) {
		ir.Constants = append(ir.Constants, ex.consts[k])
	}
	return ir
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func (ex *extractor) source() Source {
	p := ex.pkgs[protocolPkg]
	if p.Module == nil || p.Module.Path != modulePath {
		panic(failure{"gophertunnel is not loaded as a module"})
	}
	v := p.Module.Version
	if i := strings.LastIndex(v, "-"); i < 0 || !strings.HasPrefix(commit, v[i+1:]) {
		panic(failure{fmt.Sprintf("module version %s does not match commit %s", v, commit)})
	}
	scope := p.Types.Scope()
	proto, ok := constant.Int64Val(scope.Lookup("CurrentProtocol").(*types.Const).Val())
	if !ok {
		panic(failure{"CurrentProtocol is not an integer"})
	}
	return Source{
		Module:      modulePath,
		Version:     v,
		Commit:      commit,
		Protocol:    proto,
		GameVersion: constant.StringVal(scope.Lookup("CurrentVersion").(*types.Const).Val()),
	}
}

func (ex *extractor) ioMethods() []IOMethod {
	iface := ex.ioType.Underlying().(*types.Interface)
	var out []IOMethod
	for i := 0; i < iface.NumMethods(); i++ {
		m := iface.Method(i)
		sig := m.Type().(*types.Signature)
		im := IOMethod{Name: m.Name(), Params: []Param{}}
		for j := 0; j < sig.Params().Len(); j++ {
			v := sig.Params().At(j)
			im.Params = append(im.Params, Param{Name: v.Name(), Type: ex.typ(v.Type(), m.Pos())})
		}
		for j := 0; j < sig.Results().Len(); j++ {
			im.Results = append(im.Results, ex.typ(sig.Results().At(j).Type(), m.Pos()))
		}
		out = append(out, im)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// packets reads the two registration maps of packet/pool.go.
func (ex *extractor) packets() []Packet {
	p := ex.pkgs[packetPkg]
	dirs := map[uint32]string{}
	names := map[uint32]string{}
	found := map[string]bool{}
	for _, f := range p.Syntax {
		if filepath.Base(p.Fset.Position(f.Pos()).Filename) != "pool.go" {
			continue
		}
		ast.Inspect(f, func(n ast.Node) bool {
			as, ok := n.(*ast.AssignStmt)
			if !ok || len(as.Lhs) != 1 {
				return true
			}
			id, ok := as.Lhs[0].(*ast.Ident)
			if !ok {
				return true
			}
			var dir string
			switch id.Name {
			case "serverOriginating":
				dir = "server"
			case "clientOriginating":
				dir = "client"
			default:
				return true
			}
			found[dir] = true
			lit, ok := as.Rhs[0].(*ast.CompositeLit)
			if !ok {
				ex.fail(as.Pos(), "pool map %s is not a composite literal", id.Name)
			}
			for _, el := range lit.Elts {
				kv := el.(*ast.KeyValueExpr)
				tv := p.TypesInfo.Types[kv.Key]
				if tv.Value == nil {
					ex.fail(kv.Pos(), "pool key is not a constant")
				}
				v, ok := constant.Uint64Val(tv.Value)
				if !ok || v > 1<<32-1 {
					ex.fail(kv.Pos(), "pool key is not a uint32")
				}
				name := ex.poolValueType(p, kv.Value)
				key := uint32(v)
				if old, ok := names[key]; ok && old != name {
					ex.fail(kv.Pos(), "packet ID %d maps to %s and %s", key, old, name)
				}
				names[key] = name
				switch dirs[key] {
				case "":
					dirs[key] = dir
				case dir:
					ex.fail(kv.Pos(), "duplicate packet ID %d", key)
				default:
					dirs[key] = "both"
				}
			}
			return true
		})
	}
	if !found["server"] || !found["client"] {
		panic(failure{"packet/pool.go: registration maps not found"})
	}
	var out []Packet
	for id, dir := range dirs {
		out = append(out, Packet{ID: id, Type: names[id], Direction: dir})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	return out
}

// poolValueType checks a pool value is "func() Packet { return &T{} }" and returns "packet.T".
func (ex *extractor) poolValueType(p *packages.Package, e ast.Expr) string {
	fl, ok := e.(*ast.FuncLit)
	if ok && len(fl.Body.List) == 1 {
		if ret, ok := fl.Body.List[0].(*ast.ReturnStmt); ok && len(ret.Results) == 1 {
			if u, ok := ret.Results[0].(*ast.UnaryExpr); ok && u.Op == token.AND {
				if cl, ok := u.X.(*ast.CompositeLit); ok && len(cl.Elts) == 0 {
					if n, ok := p.TypesInfo.TypeOf(cl).(*types.Named); ok {
						return qualName(n.Obj())
					}
				}
			}
		}
	}
	ex.fail(e.Pos(), "pool value is not func() Packet { return &T{} }")
	return ""
}

func (ex *extractor) lookupNamed(q string) *types.Named {
	pkg, name, _ := strings.Cut(q, ".")
	path := map[string]string{"protocol": protocolPkg, "packet": packetPkg}[pkg]
	return ex.pkgs[path].Types.Scope().Lookup(name).Type().(*types.Named)
}

// qualName returns "pkgname.Name" for a package-level object.
func qualName(obj types.Object) string {
	return obj.Pkg().Name() + "." + obj.Name()
}

// local reports whether obj belongs to protocol or packet.
func local(obj types.Object) bool {
	return obj.Pkg() != nil && (obj.Pkg().Path() == protocolPkg || obj.Pkg().Path() == packetPkg)
}

func (ex *extractor) needType(n *types.Named) {
	if ex.seenTypes[n] {
		return
	}
	ex.seenTypes[n] = true
	ex.typeQueue = append(ex.typeQueue, n)
}

func (ex *extractor) needFunc(f *types.Func) {
	f = f.Origin()
	if ex.fnSeen[f] {
		return
	}
	ex.fnSeen[f] = true
	ex.fnQueue = append(ex.fnQueue, f)
}

func docText(cg *ast.CommentGroup) string {
	if cg == nil {
		return ""
	}
	return strings.TrimSpace(cg.Text())
}

// marshalMethod returns the Marshal(IO) method of *n, if any.
func (ex *extractor) marshalMethod(n *types.Named) *types.Func {
	obj, _, _ := types.LookupFieldOrMethod(types.NewPointer(n), true, n.Obj().Pkg(), "Marshal")
	f, ok := obj.(*types.Func)
	if !ok {
		return nil
	}
	if ex.isMarshalIO(f) {
		return f
	}
	return nil
}

func (ex *extractor) isMarshalIO(f *types.Func) bool {
	sig := f.Type().(*types.Signature)
	return f.Name() == "Marshal" && sig.Recv() != nil && sig.Params().Len() == 1 &&
		types.Identical(sig.Params().At(0).Type(), ex.ioType) && sig.Results().Len() == 0
}

func (ex *extractor) emitType(n *types.Named) {
	obj := n.Obj()
	name := qualName(obj)
	pos := obj.Pos()
	td := &TypeDef{Name: name, Doc: docText(ex.typeDocs[obj])}
	switch u := n.Underlying().(type) {
	case *types.Struct:
		td.Kind = "struct"
		td.Fields = []Field{}
		for i := 0; i < u.NumFields(); i++ {
			f := u.Field(i)
			td.Fields = append(td.Fields, Field{Name: f.Name(), Type: ex.typ(f.Type(), f.Pos()), Doc: docText(ex.fieldDoc[f]), Embedded: f.Embedded()})
		}
	case *types.Interface:
		td.Kind = "interface"
	case *types.Basic, *types.Array, *types.Slice, *types.Map:
		t := ex.typ(u, pos)
		td.Kind = t.Kind
		td.Underlying = &t
	default:
		ex.fail(pos, "named type %s has unsupported underlying type %s", name, u)
	}
	if m := ex.marshalMethod(n); m != nil && td.Kind == "struct" && recvNamed(m) != n {
		// Marshal is promoted from an embedded field: marshal that field.
		_, index, _ := types.LookupFieldOrMethod(types.NewPointer(n), true, obj.Pkg(), "Marshal")
		place := Expr{Kind: "var", Name: "x", Type: &Type{Kind: "named", Name: name}}
		var t types.Type = n
		for _, i := range index[:len(index)-1] {
			f := t.Underlying().(*types.Struct).Field(i)
			if _, ptr := f.Type().(*types.Pointer); ptr || !f.Embedded() {
				ex.fail(f.Pos(), "unsupported promotion of Marshal through %s", f.Name())
			}
			t = f.Type()
			prev, ft := place, ex.typ(t, f.Pos())
			place = Expr{Kind: "field", X: &prev, Name: f.Name(), Type: &ft}
		}
		emb := t.(*types.Named)
		ex.needType(emb)
		td.Marshal = &Body{Receiver: "x", Stmts: []Stmt{{Kind: "marshal", Type: qualName(emb.Obj()), Place: &place}}}
	} else if m != nil && td.Kind != "interface" {
		fi := ex.funcs[m]
		if fi == nil {
			ex.fail(pos, "Marshal of %s has no declaration", name)
		}
		c := ex.newCtx(fi)
		recv := fi.decl.Recv.List[0]
		recvName := "_"
		if len(recv.Names) == 1 {
			recvName = recv.Names[0].Name
			v := fi.pkg.TypesInfo.Defs[recv.Names[0]].(*types.Var)
			if _, ptr := v.Type().(*types.Pointer); !ptr {
				ex.fail(recv.Pos(), "Marshal of %s has a value receiver", name)
			}
			c.refs[v] = true
		}
		td.Marshal = &Body{Receiver: recvName, Stmts: c.block(fi.decl.Body.List)}
	}
	ex.types[name] = td
}

// typ converts a Go type and queues the named types it references.
func (ex *extractor) typ(t types.Type, pos token.Pos) Type {
	switch t := t.(type) {
	case *types.Basic:
		if t.Kind() == types.UnsafePointer || t.Kind() == types.Invalid || t.Kind() == types.UntypedNil {
			ex.fail(pos, "unsupported basic type %s", t)
		}
		// byte and rune are aliases; name the kind (uint8, int32).
		return Type{Kind: "basic", Name: types.Typ[t.Kind()].Name()}
	case *types.Alias:
		if t.Obj().Pkg() == nil && t.Obj().Name() == "any" {
			return Type{Kind: "any"}
		}
		return ex.typ(types.Unalias(t), pos)
	case *types.Named:
		obj := t.Obj()
		if t.Origin() == ex.optional {
			e := ex.typ(t.TypeArgs().At(0), pos)
			return Type{Kind: "optional", Elem: &e}
		}
		if t.TypeArgs().Len() > 0 {
			ex.fail(pos, "unsupported generic type %s", t)
		}
		if obj.Pkg() == nil && obj.Name() == "error" {
			return Type{Kind: "error"}
		}
		if obj.Pkg() == nil {
			ex.fail(pos, "unsupported universe type %s", t)
		}
		if !local(obj) {
			return Type{Kind: "external", Name: qualName(obj)}
		}
		if t == ex.ioType {
			ex.fail(pos, "protocol.IO used as a value type")
		}
		ex.needType(t)
		return Type{Kind: "named", Name: qualName(obj)}
	case *types.Slice:
		e := ex.typ(t.Elem(), pos)
		return Type{Kind: "slice", Elem: &e}
	case *types.Array:
		e := ex.typ(t.Elem(), pos)
		l := t.Len()
		return Type{Kind: "array", Len: &l, Elem: &e}
	case *types.Map:
		k, e := ex.typ(t.Key(), pos), ex.typ(t.Elem(), pos)
		return Type{Kind: "map", Key: &k, Elem: &e}
	case *types.Pointer:
		e := ex.typ(t.Elem(), pos)
		return Type{Kind: "pointer", Elem: &e}
	case *types.Interface:
		if t.Empty() {
			return Type{Kind: "any"}
		}
	case *types.Tuple:
		out := Type{Kind: "tuple"}
		for i := 0; i < t.Len(); i++ {
			out.Elems = append(out.Elems, ex.typ(t.At(i).Type(), pos))
		}
		return out
	}
	ex.fail(pos, "unsupported type %s", t)
	return Type{}
}

// constRef records a named constant and returns its expression.
func (ex *extractor) constRef(c *types.Const, used types.Type, pos token.Pos) Expr {
	name := qualName(c)
	if _, ok := ex.consts[name]; !ok {
		ex.consts[name] = Const{Name: name, Type: ex.typ(c.Type(), pos), Value: ex.constValue(c.Val(), pos)}
	}
	v := ex.constValue(c.Val(), pos)
	t := ex.typ(used, pos)
	return Expr{Kind: "const", Name: name, Value: &v, Type: &t}
}

func (ex *extractor) constValue(v constant.Value, pos token.Pos) string {
	switch v.Kind() {
	case constant.Int:
		return v.ExactString()
	case constant.Bool:
		return v.ExactString()
	case constant.String:
		return constant.StringVal(v)
	case constant.Float:
		f, _ := constant.Float64Val(v)
		if !constant.Compare(constant.MakeFloat64(f), token.EQL, v) {
			ex.fail(pos, "float constant %s is not exact as float64", v)
		}
		return strconv.FormatFloat(f, 'g', -1, 64)
	}
	ex.fail(pos, "unsupported constant %s", v)
	return ""
}

// recvNamed returns the named receiver type of a method, without the pointer.
func recvNamed(f *types.Func) *types.Named {
	t := f.Type().(*types.Signature).Recv().Type()
	if p, ok := t.(*types.Pointer); ok {
		t = p.Elem()
	}
	n, _ := t.(*types.Named)
	return n
}
