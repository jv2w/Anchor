package main

import (
	"go/ast"
	"go/token"
	"go/types"
	"strings"
)

// funcName returns "pkg.Func" or "pkg.Recv.Method".
func (ex *extractor) funcName(f *types.Func) string {
	f = f.Origin()
	sig := f.Type().(*types.Signature)
	if sig.Recv() == nil {
		return qualName(f)
	}
	t := sig.Recv().Type()
	if p, ok := t.(*types.Pointer); ok {
		t = p.Elem()
	}
	n, ok := t.(*types.Named)
	if !ok {
		ex.fail(f.Pos(), "method %s on unnamed type", f.Name())
	}
	return qualName(n.Obj()) + "." + f.Name()
}

// kindOf classifies a function as "union", "enum" or "function", building union and enum entries.
func (ex *extractor) kindOf(f *types.Func) string {
	f = f.Origin()
	name := ex.funcName(f)
	if _, ok := ex.unions[name]; ok {
		return "union"
	}
	if _, ok := ex.enums[name]; ok {
		return "enum"
	}
	fi := ex.funcs[f]
	if fi == nil {
		ex.fail(f.Pos(), "function %s has no declaration", name)
	}
	if u, ok := ex.tryUnion(fi); ok {
		ex.unions[name] = u
		return "union"
	}
	if strings.HasPrefix(f.Name(), "lookup") {
		ex.fail(fi.decl.Pos(), "lookup function %s does not have a union table shape", name)
	}
	if e, ok := ex.tryEnum(fi); ok {
		ex.enums[name] = e
		return "enum"
	}
	return "function"
}

func (ex *extractor) emitFunc(f *types.Func) {
	if ex.kindOf(f) != "function" {
		return
	}
	fi := ex.funcs[f]
	c := ex.newCtx(fi)
	fn := Function{Name: ex.funcName(f)}
	if fi.decl.Recv != nil {
		v := f.Type().(*types.Signature).Recv()
		p := c.param(v, fi.decl.Recv.List[0].Pos())
		fn.Receiver = &p
	}
	fn.IO, fn.Params, fn.Results = c.signature(fi)
	fn.Body = c.block(fi.decl.Body.List)
	ex.functions[fn.Name] = fn
}

// param converts a parameter, registering pointer parameters as reference bindings.
func (c *ctx) param(v *types.Var, pos token.Pos) Param {
	name := v.Name()
	if name == "" {
		name = "_"
	}
	if p, ok := v.Type().(*types.Pointer); ok {
		c.refs[v] = true
		return Param{Name: name, Type: c.ex.typ(p.Elem(), pos), Ref: true}
	}
	return Param{Name: name, Type: c.ex.typ(v.Type(), pos)}
}

// signature converts the parameters (without the IO) and results of a function.
func (c *ctx) signature(fi *funcInfo) (io bool, params []Param, results []Type) {
	sig := fi.obj.Type().(*types.Signature)
	if sig.Variadic() {
		c.fail(fi.decl, "variadic function")
	}
	params = []Param{}
	for i := 0; i < sig.Params().Len(); i++ {
		v := sig.Params().At(i)
		if c.ios[v] {
			io = true
			continue
		}
		params = append(params, c.param(v, fi.decl.Pos()))
	}
	for i := 0; i < sig.Results().Len(); i++ {
		results = append(results, c.ex.typ(sig.Results().At(i).Type(), fi.decl.Pos()))
	}
	return io, params, results
}

// paramIndex returns the index of the parameter e names, or -1.
func (c *ctx) paramIndex(sig *types.Signature, e ast.Expr) int {
	v := c.localVar(e)
	for i := 0; i < sig.Params().Len(); i++ {
		if sig.Params().At(i) == v {
			return i
		}
	}
	return -1
}

func (c *ctx) isConst(e ast.Expr, want string) bool {
	tv := c.info.Types[ast.Unparen(e)]
	return tv.Value != nil && tv.Value.ExactString() == want
}

// isReturn reports whether s is "return" with the last result being the constant want.
func (c *ctx) isReturnBool(s ast.Stmt, want string) (*ast.ReturnStmt, bool) {
	r, ok := s.(*ast.ReturnStmt)
	if !ok || len(r.Results) == 0 || !c.isConst(r.Results[len(r.Results)-1], want) {
		return nil, false
	}
	return r, true
}

// variantOf returns the struct type of "&S{}" with no fields.
func (c *ctx) variantOf(e ast.Expr) (string, bool) {
	u, ok := ast.Unparen(e).(*ast.UnaryExpr)
	if !ok || u.Op != token.AND {
		return "", false
	}
	cl, ok := u.X.(*ast.CompositeLit)
	if !ok || len(cl.Elts) != 0 {
		return "", false
	}
	n, ok := c.info.TypeOf(cl).(*types.Named)
	if !ok || !local(n.Obj()) {
		return "", false
	}
	if _, ok := n.Underlying().(*types.Struct); !ok {
		return "", false
	}
	c.ex.needType(n)
	return qualName(n.Obj()), true
}

func ifaceOf(t types.Type) (*types.Named, bool) {
	n, ok := t.(*types.Named)
	if !ok || !local(n.Obj()) {
		return nil, false
	}
	_, ok = n.Underlying().(*types.Interface)
	return n, ok
}

// derefAssign matches "*x = e" with x the parameter at index want and returns e.
func (c *ctx) derefAssign(sig *types.Signature, s ast.Stmt, want int) (ast.Expr, bool) {
	as, ok := s.(*ast.AssignStmt)
	if !ok || as.Tok != token.ASSIGN || len(as.Lhs) != 1 || len(as.Rhs) != 1 {
		return nil, false
	}
	star, ok := as.Lhs[0].(*ast.StarExpr)
	if !ok || c.paramIndex(sig, star.X) != want {
		return nil, false
	}
	return as.Rhs[0], true
}

// tryUnion matches the four lookup function shapes documented on Union.
func (ex *extractor) tryUnion(fi *funcInfo) (Union, bool) {
	c := ex.newCtx(fi)
	sig := fi.obj.Type().(*types.Signature)
	body := fi.decl.Body.List
	res := sig.Results()
	if fi.decl.Recv != nil || sig.Params().Len() == 0 || len(body) == 0 || len(body) > 2 || res.Len() == 0 {
		return Union{}, false
	}
	if b, ok := res.At(res.Len() - 1).Type().(*types.Basic); !ok || b.Kind() != types.Bool {
		return Union{}, false
	}
	if len(body) == 2 {
		if _, ok := c.isReturnBool(body[1], "true"); !ok || len(body[1].(*ast.ReturnStmt).Results) != 1 {
			return Union{}, false
		}
	}
	u := Union{Name: ex.funcName(fi.obj)}
	var iface *types.Named
	var idType types.Type
	var clauses []ast.Stmt
	switch s := body[0].(type) {
	case *ast.SwitchStmt:
		if s.Init != nil || s.Tag == nil || c.paramIndex(sig, s.Tag) != 0 {
			return Union{}, false
		}
		u.Key, idType, clauses = "id", sig.Params().At(0).Type(), s.Body.List
		if len(body) == 2 && sig.Params().Len() == 2 && res.Len() == 1 {
			p, ok := sig.Params().At(1).Type().(*types.Pointer)
			if !ok {
				return Union{}, false
			}
			iface, ok = ifaceOf(p.Elem())
			if !ok {
				return Union{}, false
			}
		} else if len(body) == 1 && sig.Params().Len() == 1 && res.Len() == 2 {
			var ok bool
			if iface, ok = ifaceOf(res.At(0).Type()); !ok {
				return Union{}, false
			}
		} else {
			return Union{}, false
		}
	case *ast.TypeSwitchStmt:
		es, ok := s.Assign.(*ast.ExprStmt)
		if !ok || s.Init != nil {
			return Union{}, false
		}
		ta := es.X.(*ast.TypeAssertExpr)
		if c.paramIndex(sig, ta.X) != 0 {
			return Union{}, false
		}
		if iface, ok = ifaceOf(sig.Params().At(0).Type()); !ok {
			return Union{}, false
		}
		u.Key, clauses = "type", s.Body.List
		if len(body) == 2 && sig.Params().Len() == 2 && res.Len() == 1 {
			p, ok := sig.Params().At(1).Type().(*types.Pointer)
			if !ok {
				return Union{}, false
			}
			idType = p.Elem()
		} else if len(body) == 1 && sig.Params().Len() == 1 && res.Len() >= 2 {
			idType = res.At(0).Type()
		} else {
			return Union{}, false
		}
	default:
		return Union{}, false
	}
	hasDefault := false
	for _, cl := range clauses {
		cc := cl.(*ast.CaseClause)
		if len(cc.Body) != 1 {
			return Union{}, false
		}
		if cc.List == nil {
			// The default must report failure and produce nothing but zero values.
			r, ok := c.isReturnBool(cc.Body[0], "false")
			if !ok {
				return Union{}, false
			}
			for _, e := range r.Results[:len(r.Results)-1] {
				if !c.isZero(e) {
					return Union{}, false
				}
			}
			hasDefault = true
			continue
		}
		var entry UnionEntry
		if u.Key == "id" {
			var variant ast.Expr
			if len(body) == 2 {
				e, ok := c.derefAssign(sig, cc.Body[0], 1)
				if !ok {
					return Union{}, false
				}
				variant = e
			} else {
				r, ok := c.isReturnBool(cc.Body[0], "true")
				if !ok || len(r.Results) != 2 {
					return Union{}, false
				}
				variant = r.Results[0]
			}
			name, ok := c.variantOf(variant)
			if !ok {
				return Union{}, false
			}
			entry.Variants = []*string{&name}
			for _, v := range cc.List {
				if c.info.Types[v].Value == nil {
					return Union{}, false
				}
				entry.IDs = append(entry.IDs, c.expr(v))
			}
		} else {
			for _, t := range cc.List {
				ct := c.caseType(t)
				if ct == nil {
					entry.Variants = append(entry.Variants, nil)
					continue
				}
				if ct.Kind != "pointer" || ct.Elem.Kind != "named" {
					return Union{}, false
				}
				name := ct.Elem.Name
				entry.Variants = append(entry.Variants, &name)
			}
			var produced []ast.Expr
			if len(body) == 2 {
				e, ok := c.derefAssign(sig, cc.Body[0], 1)
				if !ok {
					return Union{}, false
				}
				produced = []ast.Expr{e}
			} else {
				r, ok := c.isReturnBool(cc.Body[0], "true")
				if !ok {
					return Union{}, false
				}
				produced = r.Results[:len(r.Results)-1]
			}
			for i, e := range produced {
				if c.info.Types[e].Value == nil {
					return Union{}, false
				}
				if i == 0 {
					entry.IDs = []Expr{c.expr(e)}
				} else {
					entry.Extra = append(entry.Extra, c.expr(e))
				}
			}
		}
		u.Entries = append(u.Entries, entry)
	}
	if !hasDefault || len(u.Entries) == 0 {
		return Union{}, false
	}
	u.Interface = qualName(iface.Obj())
	c.ex.needType(iface)
	u.IDType = ex.typ(idType, fi.decl.Pos())
	_, u.Params, u.Results = c.signature(fi)
	return u, true
}

// isZero reports whether e is nil or a zero constant.
func (c *ctx) isZero(e ast.Expr) bool {
	e = ast.Unparen(e)
	if id, ok := e.(*ast.Ident); ok {
		if _, ok := c.info.Uses[id].(*types.Nil); ok {
			return true
		}
	}
	tv := c.info.Types[e]
	if tv.Value == nil {
		return false
	}
	s := tv.Value.ExactString()
	return s == "0" || s == `""` || s == "false"
}

// tryEnum matches a function whose body is a single switch over a parameter with constant cases.
func (ex *extractor) tryEnum(fi *funcInfo) (Enum, bool) {
	c := ex.newCtx(fi)
	sig := fi.obj.Type().(*types.Signature)
	body := fi.decl.Body.List
	if fi.decl.Recv != nil || len(body) != 1 {
		return Enum{}, false
	}
	sw, ok := body[0].(*ast.SwitchStmt)
	if !ok || sw.Init != nil || sw.Tag == nil {
		return Enum{}, false
	}
	on := c.paramIndex(sig, sw.Tag)
	if on < 0 || c.ios[sig.Params().At(on)] {
		return Enum{}, false
	}
	e := Enum{Name: ex.funcName(fi.obj), On: sig.Params().At(on).Name()}
	e.IO, e.Params, e.Results = c.signature(fi)
	for _, cl := range sw.Body.List {
		cc := cl.(*ast.CaseClause)
		if cc.List == nil {
			e.Default = c.block(cc.Body)
			continue
		}
		if len(cc.Body) != 1 {
			return Enum{}, false
		}
		var output string
		var results []ast.Expr
		switch s := cc.Body[0].(type) {
		case *ast.ReturnStmt:
			output, results = "return", s.Results
		case *ast.AssignStmt:
			if len(s.Lhs) != 1 || len(s.Rhs) != 1 || s.Tok != token.ASSIGN {
				return Enum{}, false
			}
			star, ok := s.Lhs[0].(*ast.StarExpr)
			if !ok {
				return Enum{}, false
			}
			i := c.paramIndex(sig, star.X)
			if i < 0 {
				return Enum{}, false
			}
			output, results = sig.Params().At(i).Name(), s.Rhs
		default:
			return Enum{}, false
		}
		if e.Output != "" && e.Output != output {
			return Enum{}, false
		}
		e.Output = output
		ec := EnumCase{}
		for _, r := range results {
			if c.info.Types[r].Value == nil {
				return Enum{}, false
			}
			ec.Results = append(ec.Results, c.expr(r))
		}
		for _, v := range cc.List {
			if c.info.Types[v].Value == nil {
				return Enum{}, false
			}
			ec.Values = append(ec.Values, c.expr(v))
		}
		e.Cases = append(e.Cases, ec)
	}
	if len(e.Cases) == 0 {
		return Enum{}, false
	}
	return e, true
}
