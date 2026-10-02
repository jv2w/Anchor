package main

import (
	"go/ast"
	"go/token"
	"go/types"
)

// ctx translates one function or Marshal body.
type ctx struct {
	ex   *extractor
	info *types.Info
	// ios holds the protocol.IO variables in scope.
	ios map[*types.Var]bool
	// refs holds the reference bindings: pointer variables that stand for their pointee (see ir.go, Places).
	refs map[*types.Var]bool
}

func (ex *extractor) newCtx(fi *funcInfo) *ctx {
	c := &ctx{ex: ex, info: fi.pkg.TypesInfo, ios: map[*types.Var]bool{}, refs: map[*types.Var]bool{}}
	sig := fi.obj.Type().(*types.Signature)
	for i := 0; i < sig.Params().Len(); i++ {
		v := sig.Params().At(i)
		if types.Identical(v.Type(), ex.ioType) {
			c.ios[v] = true
		}
	}
	return c
}

// helpers are the generic protocol functions translated into dedicated statements.
var helpers = map[string]bool{
	"Slice": true, "SliceUint8Length": true, "SliceUint32Length": true, "SliceOfLen": true,
	"FuncSlice": true, "FuncSliceUint32Length": true, "FuncIOSlice": true, "FuncSliceOfLen": true,
	"FuncIOSliceOfLen": true, "Single": true, "OptionalFunc": true, "OptionalMarshaler": true,
	"IntegerFunc": true,
}

// builtins are the Go builtins and standard library functions allowed in expressions.
var builtins = map[string]bool{
	"len": true, "slices.Contains": true, "fmt.Sprintf": true, "fmt.Errorf": true, "strings.EqualFold": true,
	"strings.TrimPrefix": true,
}

func (c *ctx) fail(n ast.Node, format string, args ...any) {
	c.ex.fail(n.Pos(), format, args...)
}

// typ converts the type of an expression. Comparisons have the untyped bool type in go/types; they are bool.
func (c *ctx) typ(t types.Type, n ast.Node) *Type {
	if b, ok := t.(*types.Basic); ok && b.Kind() == types.UntypedBool {
		t = types.Typ[types.Bool]
	}
	r := c.ex.typ(t, n.Pos())
	return &r
}

func (c *ctx) block(list []ast.Stmt) []Stmt {
	out := []Stmt{}
	for _, s := range list {
		out = append(out, c.stmt(s)...)
	}
	return out
}

func (c *ctx) stmt(s ast.Stmt) []Stmt {
	switch s := s.(type) {
	case *ast.ExprStmt:
		call, ok := s.X.(*ast.CallExpr)
		if !ok {
			c.fail(s, "expression statement is not a call")
		}
		return []Stmt{c.callStmt(call)}
	case *ast.AssignStmt:
		st := Stmt{Kind: "assign", Op: s.Tok.String()}
		for _, l := range s.Lhs {
			if s.Tok == token.DEFINE {
				st.Lhs = append(st.Lhs, c.define(l))
			} else {
				st.Lhs = append(st.Lhs, c.lvalue(l))
			}
		}
		for _, r := range s.Rhs {
			st.Rhs = append(st.Rhs, c.expr(r))
		}
		return []Stmt{st}
	case *ast.IncDecStmt:
		lhs := c.lvalue(s.X)
		one := "1"
		op := "+="
		if s.Tok == token.DEC {
			op = "-="
		}
		return []Stmt{{Kind: "assign", Op: op, Lhs: []Expr{lhs}, Rhs: []Expr{{Kind: "lit", Value: &one, Type: lhs.Type}}}}
	case *ast.DeclStmt:
		gd, ok := s.Decl.(*ast.GenDecl)
		if !ok || gd.Tok != token.VAR {
			c.fail(s, "unsupported declaration")
		}
		var out []Stmt
		for _, sp := range gd.Specs {
			vs := sp.(*ast.ValueSpec)
			if len(vs.Values) != 0 && len(vs.Values) != len(vs.Names) {
				c.fail(vs, "multi-value var declaration")
			}
			for i, n := range vs.Names {
				v := c.info.Defs[n].(*types.Var)
				st := Stmt{Kind: "var", Name: n.Name, VarType: c.typ(v.Type(), n)}
				if len(vs.Values) > 0 {
					e := c.expr(vs.Values[i])
					st.Value = &e
				}
				out = append(out, st)
			}
		}
		return out
	case *ast.IfStmt:
		st := Stmt{Kind: "if", Then: c.block(s.Body.List)}
		if s.Init != nil {
			st.Init = c.stmt(s.Init)
		}
		cond := c.expr(s.Cond)
		st.Cond = &cond
		switch e := s.Else.(type) {
		case nil:
		case *ast.BlockStmt:
			st.Else = c.block(e.List)
		case *ast.IfStmt:
			st.Else = c.stmt(e)
		default:
			c.fail(e, "unsupported else branch")
		}
		return []Stmt{st}
	case *ast.SwitchStmt:
		st := Stmt{Kind: "switch"}
		if s.Init != nil {
			st.Init = c.stmt(s.Init)
		}
		if s.Tag != nil {
			t := c.expr(s.Tag)
			st.Tag = &t
		}
		for _, cl := range s.Body.List {
			cc := cl.(*ast.CaseClause)
			cs := Case{Default: cc.List == nil, Body: c.block(cc.Body)}
			for _, v := range cc.List {
				cs.Values = append(cs.Values, c.expr(v))
			}
			st.Cases = append(st.Cases, cs)
		}
		return []Stmt{st}
	case *ast.TypeSwitchStmt:
		st := Stmt{Kind: "type_switch"}
		if s.Init != nil {
			st.Init = c.stmt(s.Init)
		}
		var ta *ast.TypeAssertExpr
		switch a := s.Assign.(type) {
		case *ast.ExprStmt:
			ta, _ = a.X.(*ast.TypeAssertExpr)
		case *ast.AssignStmt:
			if len(a.Lhs) == 1 && len(a.Rhs) == 1 {
				st.Bind = a.Lhs[0].(*ast.Ident).Name
				ta, _ = a.Rhs[0].(*ast.TypeAssertExpr)
			}
		}
		if ta == nil {
			c.fail(s, "unsupported type switch guard")
		}
		x := c.expr(ta.X)
		st.X = &x
		for _, cl := range s.Body.List {
			cc := cl.(*ast.CaseClause)
			cs := Case{Default: cc.List == nil, Body: c.block(cc.Body)}
			for _, t := range cc.List {
				cs.Types = append(cs.Types, c.caseType(t))
			}
			st.Cases = append(st.Cases, cs)
		}
		return []Stmt{st}
	case *ast.ForStmt:
		st := Stmt{Kind: "for", Body: c.block(s.Body.List)}
		if s.Init != nil {
			st.Init = c.stmt(s.Init)
		}
		if s.Cond != nil {
			cond := c.expr(s.Cond)
			st.Cond = &cond
		}
		if s.Post != nil {
			st.Post = c.stmt(s.Post)
		}
		return []Stmt{st}
	case *ast.RangeStmt:
		st := Stmt{Kind: "range", Define: s.Tok == token.DEFINE, Body: c.block(s.Body.List)}
		bind := func(e ast.Expr) *Expr {
			if e == nil {
				return nil
			}
			var r Expr
			if st.Define {
				r = c.define(e)
			} else {
				r = c.lvalue(e)
			}
			return &r
		}
		st.Key, st.RValue = bind(s.Key), bind(s.Value)
		x := c.base(s.X)
		st.X = &x
		return []Stmt{st}
	case *ast.ReturnStmt:
		st := Stmt{Kind: "return"}
		for _, r := range s.Results {
			st.Values = append(st.Values, c.expr(r))
		}
		return []Stmt{st}
	case *ast.EmptyStmt:
		return nil
	}
	c.fail(s, "unsupported statement %T", s)
	return nil
}

// caseType converts a type switch case type; nil is returned for the nil case.
func (c *ctx) caseType(e ast.Expr) *Type {
	if id, ok := e.(*ast.Ident); ok {
		if _, ok := c.info.Uses[id].(*types.Nil); ok {
			return nil
		}
	}
	tv := c.info.Types[e]
	if !tv.IsType() {
		c.fail(e, "type switch case is not a type")
	}
	return c.typ(tv.Type, e)
}

// define translates the left side of ":=".
func (c *ctx) define(e ast.Expr) Expr {
	id, ok := e.(*ast.Ident)
	if !ok {
		c.fail(e, "unsupported := target")
	}
	if id.Name == "_" {
		return Expr{Kind: "blank"}
	}
	v, ok := c.info.ObjectOf(id).(*types.Var)
	if !ok {
		c.fail(e, "unsupported := target")
	}
	if _, ptr := v.Type().(*types.Pointer); ptr {
		c.fail(e, "pointer local %s", id.Name)
	}
	return Expr{Kind: "var", Name: id.Name, Type: c.typ(v.Type(), e)}
}

// localVar returns the local variable an identifier refers to, or nil.
func (c *ctx) localVar(e ast.Expr) *types.Var {
	id, ok := ast.Unparen(e).(*ast.Ident)
	if !ok {
		return nil
	}
	v, ok := c.info.Uses[id].(*types.Var)
	if !ok || v.Parent() == v.Pkg().Scope() {
		return nil
	}
	return v
}

func (c *ctx) isIO(e ast.Expr) bool {
	v := c.localVar(e)
	return v != nil && c.ios[v]
}

// refVar returns the var expression of a reference binding identifier, or false.
func (c *ctx) refVar(e ast.Expr) (Expr, bool) {
	v := c.localVar(e)
	if v == nil || !c.refs[v] {
		return Expr{}, false
	}
	return Expr{Kind: "var", Name: v.Name(), Type: c.typ(v.Type().(*types.Pointer).Elem(), e)}, true
}

// place translates a pointer argument (&x or a reference binding) into the location it points to.
func (c *ctx) place(e ast.Expr) Expr {
	e = ast.Unparen(e)
	if u, ok := e.(*ast.UnaryExpr); ok && u.Op == token.AND {
		return c.lvalue(u.X)
	}
	if r, ok := c.refVar(e); ok {
		return r
	}
	c.fail(e, "pointer argument is neither &place nor a reference parameter")
	return Expr{}
}

// lvalue translates an addressable expression.
func (c *ctx) lvalue(e ast.Expr) Expr {
	e = ast.Unparen(e)
	switch x := e.(type) {
	case *ast.Ident:
		if x.Name == "_" {
			return Expr{Kind: "blank"}
		}
		if c.localVar(x) == nil {
			c.fail(x, "assignment to non-local %s", x.Name)
		}
		return c.expr(x)
	case *ast.StarExpr:
		return c.expr(x)
	case *ast.SelectorExpr, *ast.IndexExpr:
		return c.expr(x)
	}
	c.fail(e, "unsupported assignment target %T", e)
	return Expr{}
}

// base translates the operand of a field selection or dereference, allowing reference bindings.
func (c *ctx) base(e ast.Expr) Expr {
	if r, ok := c.refVar(e); ok {
		return r
	}
	return c.expr(e)
}

func (c *ctx) expr(e ast.Expr) Expr {
	e = ast.Unparen(e)
	tv, ok := c.info.Types[e]
	if !ok {
		c.fail(e, "expression without type information")
	}
	if tv.Value != nil {
		var obj types.Object
		switch x := e.(type) {
		case *ast.Ident:
			obj = c.info.Uses[x]
		case *ast.SelectorExpr:
			obj = c.info.Uses[x.Sel]
		}
		if k, ok := obj.(*types.Const); ok && k.Pkg() != nil {
			return c.ex.constRef(k, tv.Type, e.Pos())
		}
		v := c.ex.constValue(tv.Value, e.Pos())
		return Expr{Kind: "lit", Value: &v, Type: c.typ(tv.Type, e)}
	}
	switch x := e.(type) {
	case *ast.Ident:
		switch obj := c.info.Uses[x].(type) {
		case *types.Nil:
			if b, ok := tv.Type.(*types.Basic); ok && b.Kind() == types.UntypedNil {
				return Expr{Kind: "nil"}
			}
			return Expr{Kind: "nil", Type: c.typ(tv.Type, e)}
		case *types.Var:
			if c.localVar(x) == nil {
				c.fail(x, "package variable %s", x.Name)
			}
			if c.ios[obj] {
				c.fail(x, "IO used as a value")
			}
			if c.refs[obj] {
				c.fail(x, "reference parameter %s used as a pointer value", x.Name)
			}
			return Expr{Kind: "var", Name: x.Name, Type: c.typ(obj.Type(), e)}
		}
	case *ast.SelectorExpr:
		if sel, ok := c.info.Selections[x]; ok {
			if sel.Kind() != types.FieldVal {
				c.fail(x, "unsupported selector %s", x.Sel.Name)
			}
			// Promoted fields are spelled out through their embedded fields.
			b := c.base(x.X)
			t := sel.Recv()
			for _, i := range sel.Index() {
				if p, ok := t.Underlying().(*types.Pointer); ok {
					t = p.Elem()
				}
				f := t.Underlying().(*types.Struct).Field(i)
				t = f.Type()
				prev := b
				b = Expr{Kind: "field", X: &prev, Name: f.Name(), Type: c.typ(t, e)}
			}
			return b
		}
		if v, ok := c.info.Uses[x.Sel].(*types.Var); ok && !local(v) {
			return Expr{Kind: "external", Name: qualName(v), Type: c.typ(tv.Type, e)}
		}
	case *ast.CallExpr:
		return c.callExpr(x)
	case *ast.BinaryExpr:
		l, r := c.expr(x.X), c.expr(x.Y)
		return Expr{Kind: "binary", Op: x.Op.String(), X: &l, Y: &r, Type: c.typ(tv.Type, e)}
	case *ast.UnaryExpr:
		switch x.Op {
		case token.AND:
			if cl, ok := ast.Unparen(x.X).(*ast.CompositeLit); ok {
				r := c.composite(cl)
				r.Kind = "new"
				r.Type = c.typ(tv.Type, e)
				return r
			}
		case token.NOT, token.SUB, token.XOR:
			o := c.expr(x.X)
			return Expr{Kind: "unary", Op: x.Op.String(), X: &o, Type: c.typ(tv.Type, e)}
		}
	case *ast.StarExpr:
		if r, ok := c.refVar(x.X); ok {
			return r
		}
		o := c.expr(x.X)
		return Expr{Kind: "deref", X: &o, Type: c.typ(tv.Type, e)}
	case *ast.IndexExpr:
		if _, ok := c.info.Instances[indexIdent(x.X)]; !ok {
			b, i := c.base(x.X), c.expr(x.Index)
			return Expr{Kind: "index", X: &b, Index: &i, Type: c.typ(tv.Type, e)}
		}
	case *ast.SliceExpr:
		if !x.Slice3 {
			b := c.expr(x.X)
			r := Expr{Kind: "slice_of", X: &b, Type: c.typ(tv.Type, e)}
			if x.Low != nil {
				l := c.expr(x.Low)
				r.Low = &l
			}
			if x.High != nil {
				h := c.expr(x.High)
				r.High = &h
			}
			return r
		}
	case *ast.CompositeLit:
		return c.composite(x)
	}
	c.fail(e, "unsupported expression %T", e)
	return Expr{}
}

func indexIdent(e ast.Expr) *ast.Ident {
	switch x := e.(type) {
	case *ast.Ident:
		return x
	case *ast.SelectorExpr:
		return x.Sel
	}
	return nil
}

// composite translates a keyed struct literal.
func (c *ctx) composite(cl *ast.CompositeLit) Expr {
	t := c.info.TypeOf(cl)
	r := Expr{Kind: "composite", Type: c.typ(t, cl)}
	if n, ok := t.(*types.Named); ok && n.Origin() == c.ex.optional && len(cl.Elts) != 0 {
		c.fail(cl, "protocol.Optional literal with fields")
	}
	switch t.Underlying().(type) {
	case *types.Struct:
	case *types.Array, *types.Slice:
		for _, el := range cl.Elts {
			if _, ok := el.(*ast.KeyValueExpr); ok {
				c.fail(el, "keyed array or slice literal element")
			}
			r.Elems = append(r.Elems, c.expr(el))
		}
		return r
	default:
		c.fail(cl, "composite literal of unsupported type %s", t)
	}
	for _, el := range cl.Elts {
		kv, ok := el.(*ast.KeyValueExpr)
		if !ok {
			c.fail(el, "unkeyed composite literal field")
		}
		r.Fields = append(r.Fields, FieldValue{Name: kv.Key.(*ast.Ident).Name, Value: c.expr(kv.Value)})
	}
	return r
}

// callee resolves the function called, unwrapping explicit instantiation.
func (c *ctx) callee(call *ast.CallExpr) (ast.Expr, *ast.Ident) {
	fun := ast.Unparen(call.Fun)
	switch f := fun.(type) {
	case *ast.IndexExpr:
		fun = f.X
	case *ast.IndexListExpr:
		fun = f.X
	}
	return fun, indexIdent(fun)
}

func (c *ctx) callExpr(call *ast.CallExpr) Expr {
	tv := c.info.Types[call]
	if c.info.Types[call.Fun].IsType() {
		if len(call.Args) != 1 {
			c.fail(call, "conversion with %d arguments", len(call.Args))
		}
		x := c.expr(call.Args[0])
		return Expr{Kind: "convert", X: &x, Type: c.typ(tv.Type, call)}
	}
	fun, id := c.callee(call)
	if id == nil {
		c.fail(call, "unsupported callee %T", fun)
	}
	switch obj := c.info.Uses[id].(type) {
	case *types.Builtin:
		if !builtins[obj.Name()] {
			c.fail(call, "unsupported builtin %s", obj.Name())
		}
		return Expr{Kind: "call", Func: obj.Name(), Target: "builtin", Args: c.exprs(call.Args), Type: c.typ(tv.Type, call)}
	case *types.Func:
		sel, isSel := fun.(*ast.SelectorExpr)
		if isSel && c.isIO(sel.X) {
			if obj.Name() != "ShieldID" {
				c.fail(call, "IO method %s used as an expression", obj.Name())
			}
			return Expr{Kind: "shield_id", Type: c.typ(tv.Type, call)}
		}
		if obj.Pkg() == nil && obj.Name() == "Error" && isSel {
			x := c.expr(sel.X)
			return Expr{Kind: "call", Func: "error.Error", Target: "builtin", Recv: &x, Type: c.typ(tv.Type, call)}
		}
		if !local(obj) {
			name := qualName(obj)
			if !builtins[name] {
				c.fail(call, "unsupported external function %s", name)
			}
			return Expr{Kind: "call", Func: name, Target: "builtin", Args: c.exprs(call.Args), Type: c.typ(tv.Type, call)}
		}
		if obj.Pkg().Path() == protocolPkg && obj.Name() == "Option" {
			x := c.expr(call.Args[0])
			return Expr{Kind: "option", X: &x, Type: c.typ(tv.Type, call)}
		}
		sig := obj.Type().(*types.Signature)
		if sig.Recv() != nil {
			if n, ok := sig.Recv().Type().(*types.Named); ok && n.Origin() == c.ex.optional && obj.Name() == "Value" {
				x := c.expr(sel.X)
				return Expr{Kind: "optional_value", X: &x, Type: c.typ(tv.Type, call)}
			}
		}
		r := c.localCall(call, obj)
		r.Type = c.typ(tv.Type, call)
		return r
	}
	c.fail(call, "unsupported call")
	return Expr{}
}

func (c *ctx) exprs(list []ast.Expr) []Expr {
	var out []Expr
	for _, e := range list {
		out = append(out, c.expr(e))
	}
	return out
}

// localCall translates a call of a protocol or packet function or method (not Marshal(IO)).
func (c *ctx) localCall(call *ast.CallExpr, obj *types.Func) Expr {
	if helpers[obj.Name()] && obj.Pkg().Path() == protocolPkg {
		c.fail(call, "generic helper %s used as an expression", obj.Name())
	}
	fi := c.ex.funcs[obj.Origin()]
	if fi == nil {
		c.fail(call, "function %s has no declaration", obj.Name())
	}
	sig := obj.Type().(*types.Signature)
	if sig.Variadic() {
		c.fail(call, "variadic call of %s", obj.Name())
	}
	r := Expr{Kind: "call", Func: c.ex.funcName(obj), Target: c.ex.kindOf(obj)}
	if sig.Recv() != nil {
		sel, ok := ast.Unparen(call.Fun).(*ast.SelectorExpr)
		if !ok {
			c.fail(call, "method call without selector")
		}
		if _, ok := sig.Recv().Type().(*types.Pointer); ok {
			p := c.recvPlace(sel.X)
			r.Recv = &p
		} else {
			x := c.base(sel.X)
			r.Recv = &x
		}
	}
	for i, a := range call.Args {
		r.Args = append(r.Args, c.arg(sig.Params().At(i).Type(), a)...)
	}
	c.ex.needFunc(obj)
	return r
}

// arg translates a call argument for a parameter of type t; the IO argument is dropped.
func (c *ctx) arg(t types.Type, a ast.Expr) []Expr {
	if types.Identical(t, c.ex.ioType) {
		if !c.isIO(a) {
			c.fail(a, "IO argument is not the current IO")
		}
		return nil
	}
	if _, ok := t.(*types.Pointer); ok {
		return []Expr{c.place(a)}
	}
	return []Expr{c.expr(a)}
}

// recvPlace translates the receiver of a pointer method: an addressable value or a reference binding.
func (c *ctx) recvPlace(e ast.Expr) Expr {
	if r, ok := c.refVar(e); ok {
		return r
	}
	if t := c.info.TypeOf(e); t != nil {
		if _, ptr := t.(*types.Pointer); ptr {
			c.fail(e, "pointer receiver value")
		}
	}
	return c.lvalue(e)
}

func (c *ctx) callStmt(call *ast.CallExpr) Stmt {
	fun, id := c.callee(call)
	if id == nil {
		c.fail(call, "unsupported callee %T", fun)
	}
	obj, ok := c.info.Uses[id].(*types.Func)
	if !ok {
		c.fail(call, "call statement of a non-function")
	}
	if sel, ok := fun.(*ast.SelectorExpr); ok && c.isIO(sel.X) {
		return c.ioCall(call, obj.Name())
	}
	if !local(obj) {
		c.fail(call, "call statement of external function %s", qualName(obj))
	}
	if helpers[obj.Name()] && obj.Pkg().Path() == protocolPkg {
		return c.helper(call, obj.Name(), id)
	}
	if c.ex.isMarshalIO(obj) {
		sel := fun.(*ast.SelectorExpr)
		if len(call.Args) != 1 || !c.isIO(call.Args[0]) {
			c.fail(call, "Marshal called with another IO")
		}
		var p Expr
		if star, ok := ast.Unparen(sel.X).(*ast.StarExpr); ok {
			p = c.expr(star)
		} else {
			p = c.recvPlace(sel.X)
		}
		return c.marshal(call, c.info.TypeOf(sel.X), p)
	}
	r := c.localCall(call, obj)
	return Stmt{Kind: "call", Func: r.Func, Target: r.Target, Recv: r.Recv, Args: r.Args}
}

// ioCall translates io.Method(...).
func (c *ctx) ioCall(call *ast.CallExpr, method string) Stmt {
	str := func(i int) *Expr {
		e := c.expr(call.Args[i])
		return &e
	}
	switch method {
	case "InvalidValue":
		v := c.expr(call.Args[0])
		return Stmt{Kind: "error", Error: "invalid_value", Value: &v, Field: str(1), Reason: str(2)}
	case "UnknownEnumOption":
		v := c.expr(call.Args[0])
		return Stmt{Kind: "error", Error: "unknown_enum_option", Value: &v, Enum: str(1)}
	case "ShieldID":
		c.fail(call, "ShieldID result unused")
	}
	if len(call.Args) == 0 {
		c.fail(call, "IO method %s without arguments", method)
	}
	p := c.place(call.Args[0])
	return Stmt{Kind: "io", Method: method, Place: &p, Args: c.exprs(call.Args[1:])}
}

// marshal returns a marshal statement for a value of type t (a pointer is dereferenced) at place p.
func (c *ctx) marshal(n ast.Node, t types.Type, p Expr) Stmt {
	if ptr, ok := t.(*types.Pointer); ok {
		t = ptr.Elem()
	}
	named, ok := t.(*types.Named)
	if !ok || !local(named.Obj()) {
		c.fail(n, "marshal of unsupported type %s", t)
	}
	_, isIface := named.Underlying().(*types.Interface)
	if !isIface && c.ex.marshalMethod(named) == nil {
		c.fail(n, "type %s has no Marshal(IO)", t)
	}
	c.ex.needType(named)
	return Stmt{Kind: "marshal", Type: qualName(named.Obj()), Place: &p, Dynamic: isIface}
}

// helper translates a call of one of the generic protocol helpers.
func (c *ctx) helper(call *ast.CallExpr, name string, id *ast.Ident) Stmt {
	inst, ok := c.info.Instances[id]
	if !ok {
		c.fail(call, "uninstantiated helper %s", name)
	}
	elemT := inst.TypeArgs.At(0)
	args := call.Args
	if name == "IntegerFunc" {
		p := c.place(args[0])
		elem, body := c.callback(args[1], inst.TypeArgs.At(1), false)
		return Stmt{Kind: "integer_func", Place: &p, Elem: elem, Body: body}
	}
	if !c.isIO(args[0]) {
		c.fail(call, "%s called with another IO", name)
	}
	marshalElem := func() (*Param, []Stmt) {
		v := Expr{Kind: "var", Name: "v", Type: c.typ(elemT, call)}
		return &Param{Name: "v", Type: *v.Type, Ref: true}, []Stmt{c.marshal(call, elemT, v)}
	}
	st := Stmt{Kind: "slice"}
	switch name {
	case "Single":
		return c.marshal(call, elemT, c.place(args[1]))
	case "OptionalFunc", "OptionalMarshaler":
		p := c.place(args[1])
		st = Stmt{Kind: "optional", Place: &p}
		if name == "OptionalFunc" {
			st.Elem, st.Body = c.callback(args[2], elemT, false)
		} else {
			st.Elem, st.Body = marshalElem()
		}
		return st
	case "Slice", "SliceUint8Length", "SliceUint32Length":
		st.Prefix = map[string]string{"Slice": "varuint32", "SliceUint8Length": "uint8", "SliceUint32Length": "uint32"}[name]
		p := c.place(args[1])
		st.Place = &p
		st.Elem, st.Body = marshalElem()
	case "FuncSlice", "FuncIOSlice", "FuncSliceUint32Length":
		st.Prefix = "varuint32"
		if name == "FuncSliceUint32Length" {
			st.Prefix = "uint32"
		}
		p := c.place(args[1])
		st.Place = &p
		st.Elem, st.Body = c.callback(args[2], elemT, name == "FuncIOSlice")
	case "SliceOfLen", "FuncSliceOfLen", "FuncIOSliceOfLen":
		l := c.expr(args[1])
		p := c.place(args[2])
		st.Len, st.Place = &l, &p
		if name == "SliceOfLen" {
			st.Elem, st.Body = marshalElem()
		} else {
			st.Elem, st.Body = c.callback(args[3], elemT, name == "FuncIOSliceOfLen")
		}
	}
	return st
}

// callback normalises a callback argument (func(*T) or, with withIO, func(IO, *T)) into an element binding
// and a body acting on it.
func (c *ctx) callback(f ast.Expr, elemT types.Type, withIO bool) (*Param, []Stmt) {
	f = ast.Unparen(f)
	elem := &Param{Name: "v", Type: *c.typ(elemT, f), Ref: true}
	v := Expr{Kind: "var", Name: "v", Type: &elem.Type}
	if lit, ok := f.(*ast.FuncLit); ok {
		sig := c.info.TypeOf(lit).(*types.Signature)
		want := 1
		if withIO {
			want = 2
		}
		if sig.Params().Len() != want || sig.Results().Len() != 0 {
			c.fail(lit, "callback literal has the wrong signature")
		}
		if withIO {
			c.ios[sig.Params().At(0)] = true
		}
		p := sig.Params().At(want - 1)
		ptr, ok := p.Type().(*types.Pointer)
		if !ok || !types.Identical(ptr.Elem(), elemT) || p.Name() == "" || p.Name() == "_" {
			c.fail(lit, "callback literal parameter is not a named *%s", elemT)
		}
		c.refs[p] = true
		elem.Name = p.Name()
		return elem, c.block(lit.Body.List)
	}
	sel, isSel := f.(*ast.SelectorExpr)
	if isSel && c.isIO(sel.X) {
		if withIO {
			c.fail(f, "IO method value used as func(IO, *T)")
		}
		return elem, []Stmt{{Kind: "io", Method: sel.Sel.Name, Place: &v}}
	}
	id := indexIdent(f)
	if id == nil {
		c.fail(f, "unsupported callback %T", f)
	}
	obj, ok := c.info.Uses[id].(*types.Func)
	if !ok || !local(obj) {
		c.fail(f, "unsupported callback")
	}
	sig := obj.Type().(*types.Signature)
	n := 1
	if withIO {
		n = 2
	}
	if sig.Params().Len() != n || (withIO && !types.Identical(sig.Params().At(0).Type(), c.ex.ioType)) {
		c.fail(f, "callback %s has the wrong signature", obj.Name())
	}
	st := Stmt{Kind: "call", Func: c.ex.funcName(obj), Target: c.ex.kindOf(obj), Args: []Expr{v}}
	if sig.Recv() != nil {
		if !isSel {
			c.fail(f, "method value without selector")
		}
		if _, ptr := sig.Recv().Type().(*types.Pointer); ptr {
			c.fail(f, "pointer method value as callback")
		}
		r := c.expr(sel.X)
		st.Recv = &r
	}
	c.ex.needFunc(obj)
	return elem, []Stmt{st}
}
