package main

import (
	"go/ast"
	"go/constant"
	"go/token"
	"go/types"
	"path/filepath"
	"sort"
	"strconv"
)

// constDecls lists the const declarations of protocol and packet that declare an exported name (see
// ConstDecl). It fails on a constant whose type or value it cannot describe.
func (ex *extractor) constDecls() []ConstDecl {
	type located struct {
		decl ConstDecl
		pos  token.Position
	}
	var all []located
	for _, path := range []string{protocolPkg, packetPkg} {
		p := ex.pkgs[path]
		for _, f := range p.Syntax {
			for _, d := range f.Decls {
				g, ok := d.(*ast.GenDecl)
				if !ok || g.Tok != token.CONST {
					continue
				}
				decl := ConstDecl{Package: p.Types.Name(), File: filepath.Base(p.Fset.Position(g.Pos()).Filename),
					Consts: []DeclConst{}}
				// A spec without values repeats the previous value expressions (with the next iota).
				var values []ast.Expr
				for _, s := range g.Specs {
					vs := s.(*ast.ValueSpec)
					if len(vs.Values) > 0 {
						values = vs.Values
					}
					for i, id := range vs.Names {
						if !id.IsExported() {
							continue
						}
						c := p.TypesInfo.Defs[id].(*types.Const)
						var value ast.Expr
						if i < len(values) {
							value = values[i]
						}
						dc := ex.declConst(p.TypesInfo, c, value, vs)
						if dc.Doc == "" && !g.Lparen.IsValid() {
							dc.Doc = docText(g.Doc)
						}
						decl.Consts = append(decl.Consts, dc)
					}
				}
				if len(decl.Consts) > 0 {
					all = append(all, located{decl, p.Fset.Position(g.Pos())})
				}
			}
		}
	}
	sort.Slice(all, func(i, j int) bool {
		a, b := all[i], all[j]
		if a.decl.Package != b.decl.Package {
			return a.decl.Package > b.decl.Package // protocol before packet
		}
		if a.decl.File != b.decl.File {
			return a.decl.File < b.decl.File
		}
		return a.pos.Offset < b.pos.Offset
	})
	out := make([]ConstDecl, 0, len(all))
	for _, l := range all {
		out = append(out, l.decl)
	}
	return out
}

// declConst describes one exported constant; value is its (possibly repeated) value expression.
func (ex *extractor) declConst(info *types.Info, c *types.Const, value ast.Expr, vs *ast.ValueSpec) DeclConst {
	pos := c.Pos()
	dc := DeclConst{Name: qualName(c), Doc: docText(vs.Doc)}
	if dc.Doc == "" {
		dc.Doc = docText(vs.Comment)
	}
	var basic *types.Basic
	switch t := c.Type().(type) {
	case *types.Basic:
		basic = t
		dc.Type = types.Typ[t.Kind()].Name()
	case *types.Named:
		b, ok := t.Underlying().(*types.Basic)
		if !ok || !local(t.Obj()) {
			ex.fail(pos, "constant %s has unsupported type %s", c.Name(), t)
		}
		basic = b
		dc.Type = qualName(t.Obj())
	default:
		ex.fail(pos, "constant %s has unsupported type %s", c.Name(), t)
	}
	dc.Basic = types.Typ[basic.Kind()].Name()
	v := c.Val()
	switch v.Kind() {
	case constant.Int:
		dc.Value = v.ExactString()
	case constant.String:
		dc.Value = constant.StringVal(v)
	case constant.Float:
		f, _ := constant.Float64Val(v)
		s := strconv.FormatFloat(f, 'g', -1, 64)
		if !constant.Compare(constant.MakeFromLiteral(s, token.FLOAT, 0), token.EQL, v) {
			ex.fail(pos, "float constant %s = %s has no exact short decimal", c.Name(), v.ExactString())
		}
		dc.Value = s
	default:
		ex.fail(pos, "constant %s has unsupported value %s", c.Name(), v)
	}
	if id, ok := value.(*ast.Ident); ok {
		if other, ok := info.Uses[id].(*types.Const); ok && local(other) {
			dc.Alias = qualName(other)
		}
	}
	return dc
}
