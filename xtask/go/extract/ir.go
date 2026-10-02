package main

// This file is the schema of the protocol IR written to xtask/data/protocol.json. It is the only
// documentation of the IR: every JSON object in the output is one of the types below, encoded with
// encoding/json (field names are the json tags, empty optional fields are omitted).
//
// # Overview
//
// The IR describes the gophertunnel packet codecs as data. gophertunnel encodes and decodes with the same
// code: a Marshal method receives a protocol.IO, which is either a Reader (fills the places passed) or a
// Writer (writes the values found at the places passed). The IR keeps that symmetry: a statement such as
// {"kind":"io","method":"Varuint32","place":...} means "read into the place" for a decoder and "write the
// place" for an encoder. Code that behaves differently for the two directions is only reachable through
// the explicit constructs documented below; there is none at the pinned commit outside the IO methods
// themselves, which are hand-written codecs (see IOMethod).
//
// # Names
//
// Every named Go entity is qualified with the last element of its package path: "protocol.BlockPos",
// "packet.Text", "protocol.CommandOriginData", "packet.IDText". Types from other modules are externals,
// named by package name and type name ("mgl32.Vec3", "uuid.UUID", "color.RGBA", "big.Int",
// "nbt.Encoding"). Function, enum and union names share one namespace.
//
// # Places
//
// gophertunnel passes pointers to locations ("&pk.Field"). The IR drops the pointer and writes the location
// itself, called a place: an Expr of kind "var", "field" or "index" (or "deref" of a pointer field). A
// pointer parameter of a function, or the element bound by a slice, optional or integer_func callback, is a
// reference binding (Param.Ref = true): the variable names the pointee itself, it is never a pointer value,
// and assigning to it assigns the pointee. Field access through Go pointers is implicit.
//
// # IO
//
// The IO value is implicit. Functions that take a protocol.IO have Function.IO = true and do not list it in
// Params; calls to them do not pass it either. Statements of kind "io", "marshal", "slice", "optional",
// "integer_func" and "error" always act on the current IO.
//
// # Scoping
//
// Local names are the Go names. Blocks (if/else/case/for bodies) open a new scope and Go shadowing rules
// apply. Statements run in order; a "return" leaves the current function or Marshal body.
//
// # Values
//
// Constant values (Const.Value, Expr.Value) are strings: decimal integers, Go "g" formatted floats,
// "true"/"false", or the raw string content. The accompanying Type tells how to parse them.

// IR is the root object of protocol.json.
type IR struct {
	// Source identifies the gophertunnel snapshot the IR was extracted from.
	Source Source `json:"source"`
	// IOMethods lists the methods of protocol.IO, sorted by name. Each "io" statement calls one of them.
	IOMethods []IOMethod `json:"io_methods"`
	// Packets lists every packet registered in packet/pool.go, sorted by ID.
	Packets []Packet `json:"packets"`
	// Types lists every named type reachable from the packets, the union variants and the IO method
	// signatures, sorted by name.
	Types []TypeDef `json:"types"`
	// Functions lists every function and method (other than Marshal(IO)) called from an emitted body that is
	// neither an enum nor a union, sorted by name.
	Functions []Function `json:"functions"`
	// Enums lists the switch-table conversion functions (value <-> string), sorted by name.
	Enums []Enum `json:"enums"`
	// Unions lists the interface variant tables (lookup functions), sorted by name.
	Unions []Union `json:"unions"`
	// Constants lists every named constant referenced anywhere in the IR, sorted by name.
	Constants []Const `json:"constants"`
}

// Source identifies the extracted module snapshot.
type Source struct {
	// Module is the Go module path ("github.com/sandertv/gophertunnel").
	Module string `json:"module"`
	// Version is the Go module version (a pseudo-version for a commit pin).
	Version string `json:"version"`
	// Commit is the full git commit hash of the snapshot.
	Commit string `json:"commit"`
	// Protocol is protocol.CurrentProtocol.
	Protocol int64 `json:"protocol"`
	// GameVersion is protocol.CurrentVersion.
	GameVersion string `json:"game_version"`
}

// IOMethod is one method of the protocol.IO interface. Methods whose first parameter is a pointer are
// called by "io" statements with the pointee as Stmt.Place; the remaining parameters are Stmt.Args.
// ShieldID, UnknownEnumOption and InvalidValue are not "io" statements: they appear as the "shield_id"
// expression and the "error" statement.
type IOMethod struct {
	// Name is the method name ("Varuint32").
	Name string `json:"name"`
	// Params are the parameters in order, with their Go types (the first one is usually a pointer).
	Params []Param `json:"params"`
	// Results are the result types, if any.
	Results []Type `json:"results,omitempty"`
}

// Packet is one registered packet.
type Packet struct {
	// ID is the packet ID (the resolved ID constant).
	ID uint32 `json:"id"`
	// Type is the packet struct ("packet.Text"); its definition and Marshal body are in IR.Types.
	Type string `json:"type"`
	// Direction is "client" (registered only in the client pool: sent by clients), "server" (only in the
	// server pool: sent by servers) or "both".
	Direction string `json:"direction"`
}

// TypeDef is one named Go type.
type TypeDef struct {
	// Name is the qualified type name ("protocol.BlockPos").
	Name string `json:"name"`
	// Doc is the doc comment of the type, trimmed, or empty.
	Doc string `json:"doc,omitempty"`
	// Kind is "struct" (Fields is set), "interface" (a union interface; see IR.Unions) or "basic", "array",
	// "slice", "map" for named non-struct types (Underlying is set).
	Kind string `json:"kind"`
	// Underlying is the underlying type of a non-struct named type ("type BlockPos [3]int32").
	Underlying *Type `json:"underlying,omitempty"`
	// Fields are the struct fields in declaration order, including unexported ones.
	Fields []Field `json:"fields,omitempty"`
	// Marshal is the body of the type's Marshal(IO) method, or absent when the type has none. A type with
	// an empty Marshal method has Marshal set to an empty list.
	Marshal *Body `json:"marshal,omitempty"`
}

// Body is a Marshal method body.
type Body struct {
	// Receiver is the receiver variable name; it is a reference binding to the value being marshaled.
	Receiver string `json:"receiver"`
	// Stmts are the statements of the body.
	Stmts []Stmt `json:"stmts"`
}

// Field is one struct field.
type Field struct {
	// Name is the Go field name.
	Name string `json:"name"`
	// Type is the field type.
	Type Type `json:"type"`
	// Doc is the field doc comment, trimmed, or empty.
	Doc string `json:"doc,omitempty"`
	// Embedded marks an embedded field (Name is then the type name). Bodies never use promoted fields: they
	// select through the embedded field explicitly (x.AttributeValue.Min).
	Embedded bool `json:"embedded,omitempty"`
}

// Type is a structured Go type. Kind selects the meaning of the other fields:
//
//	{"kind":"basic","name":"uint32"}            bool, uint8 (byte is uint8), int8, uint16, int16, uint32, int32,
//	                                           uint64, int64, int, uint, float32, float64, string, or
//	                                           "untyped int"/"untyped float"/"untyped string"/"untyped bool"
//	                                           for declared constant types (Const.Type) and untyped constants
//	                                           that stay untyped in an expression
//	{"kind":"named","name":"protocol.BlockPos"} a type in protocol or packet (see IR.Types)
//	{"kind":"external","name":"mgl32.Vec3"}     a named type from another package
//	{"kind":"any"}                             any (interface{})
//	{"kind":"error"}                           the error interface (a message; see "call" of "error.Error")
//	{"kind":"slice","elem":T}
//	{"kind":"array","len":3,"elem":T}
//	{"kind":"map","key":K,"elem":T}
//	{"kind":"pointer","elem":T}
//	{"kind":"optional","elem":T}               protocol.Optional[T]: a bool, then T when the bool is true
//	{"kind":"tuple","elems":[T...]}            the result of a call with several results
type Type struct {
	Kind  string `json:"kind"`
	Name  string `json:"name,omitempty"`
	Len   *int64 `json:"len,omitempty"`
	Key   *Type  `json:"key,omitempty"`
	Elem  *Type  `json:"elem,omitempty"`
	Elems []Type `json:"elems,omitempty"`
}

// Param is a function or callback parameter.
type Param struct {
	// Name is the Go parameter name ("_" when unnamed).
	Name string `json:"name"`
	// Type is the parameter type. For a reference binding it is the pointee type.
	Type Type `json:"type"`
	// Ref marks a reference binding: the Go parameter is a pointer, and the name stands for the pointee.
	// Callers pass a place.
	Ref bool `json:"ref,omitempty"`
}

// Function is a free function, or a method other than Marshal(IO), called from an emitted body.
type Function struct {
	// Name is the qualified name: "protocol.CommandOriginData", or "protocol.CommandEnumContext.Marshal" for
	// a method.
	Name string `json:"name"`
	// Doc is the doc comment, trimmed, or empty.
	Doc string `json:"doc,omitempty"`
	// Receiver is the method receiver, absent for free functions. It is passed as Stmt.Recv / Expr.Recv.
	Receiver *Param `json:"receiver,omitempty"`
	// IO is true when the function takes the protocol.IO (left out of Params and call arguments).
	IO bool `json:"io"`
	// Params are the parameters without the IO.
	Params []Param `json:"params"`
	// Results are the result types.
	Results []Type `json:"results,omitempty"`
	// Body is the statement list.
	Body []Stmt `json:"body"`
}

// Enum is a switch-table function: its whole body is one switch over a parameter (On) whose every case is
// a single "return" of constants (Output = "return") or a single assignment of a constant to a reference
// parameter (Output = the parameter name). Calls use the same rules as Function calls.
type Enum struct {
	// Name is the qualified function name ("packet.clientBoundDebugRenderToString").
	Name string `json:"name"`
	// IO is true when the function takes the protocol.IO (only used by Default).
	IO bool `json:"io"`
	// Params are the parameters without the IO.
	Params []Param `json:"params"`
	// Results are the result types.
	Results []Type `json:"results,omitempty"`
	// On is the name of the parameter switched on.
	On string `json:"on"`
	// Output is "return", or the name of the reference parameter each case assigns.
	Output string `json:"output"`
	// Cases are the switch cases in source order.
	Cases []EnumCase `json:"cases"`
	// Default is the default case body (for example a "return" of "unknown" or an "error" statement), or
	// absent when the switch has no default (nothing happens).
	Default []Stmt `json:"default,omitempty"`
}

// EnumCase maps the case values to the results.
type EnumCase struct {
	// Values are the case values (constant or literal expressions) matched against the On parameter.
	Values []Expr `json:"values"`
	// Results are the returned values (Output = "return") or the single assigned value.
	Results []Expr `json:"results"`
}

// Union is an interface variant table, taken from a lookup function. Key "id" means the function maps an
// ID to a fresh variant ("switch id { case C: *x = &S{} }" or "return &S{}, true"); key "type" means it
// maps the dynamic type of an interface value to an ID ("switch x.(type) { case *S: *id = C }" or
// "return C, extra..., true"). Every function reports success with a final bool; unknown keys report false
// and leave the outputs untouched.
type Union struct {
	// Name is the qualified function name ("protocol.lookupEvent").
	Name string `json:"name"`
	// Interface is the union interface type ("protocol.Event").
	Interface string `json:"interface"`
	// IDType is the type of the ID.
	IDType Type `json:"id_type"`
	// Key is "id" or "type" (see Union).
	Key string `json:"key"`
	// Params are the function parameters (a reference parameter is an output).
	Params []Param `json:"params"`
	// Results are the function results (the last one is the success bool).
	Results []Type `json:"results"`
	// Entries are the table rows in source order.
	Entries []UnionEntry `json:"entries"`
}

// UnionEntry is one row of a Union.
type UnionEntry struct {
	// IDs are the IDs of the row: all case values for Key "id", the one produced ID for Key "type".
	IDs []Expr `json:"ids"`
	// Variants are the variant struct types of the row: the one produced variant for Key "id", all case
	// types for Key "type". The JSON null entry stands for a nil interface value.
	Variants []*string `json:"variants"`
	// Extra are further produced values besides the ID and the bool, in result order (Key "type" only).
	Extra []Expr `json:"extra,omitempty"`
}

// Const is a named constant.
type Const struct {
	// Name is the qualified name ("packet.TextTypeChat").
	Name string `json:"name"`
	// Type is the declared type (often "untyped int" for iota constants).
	Type Type `json:"type"`
	// Value is the exact value (see "Values" above).
	Value string `json:"value"`
}

// Stmt is one statement. Kind selects the meaning of the other fields:
//
//	io            Method(Place, Args...): call an IO method on the place.
//	              {"kind":"io","method":"Varuint32","place":P}
//	              {"kind":"io","method":"NBT","place":P,"args":[{"kind":"external","name":"nbt.NetworkLittleEndian"}]}
//	              {"kind":"io","method":"Bitset","place":P,"args":[E]}
//	marshal       Marshal the place with its type's Marshal body (Type). Dynamic is true when the static
//	              type is a union interface: the dynamic variant's Marshal runs. Covers x.Marshal(io) and
//	              protocol.Single.
//	              {"kind":"marshal","type":"protocol.BlockPos","place":P}
//	slice         A list. Prefix is the count encoding written before the elements: "varuint32"
//	              (Slice, FuncSlice, FuncIOSlice), "uint8" (SliceUint8Length), "uint32" (SliceUint32Length,
//	              FuncSliceUint32Length), or absent with Len set (SliceOfLen, FuncSliceOfLen,
//	              FuncIOSliceOfLen: Len is the element count; no prefix). A reader checks the count against
//	              the slice limit (1024) and allocates that many elements. Then Body runs once per element
//	              with Elem bound (reference binding) to the element.
//	              {"kind":"slice","prefix":"varuint32","place":P,"elem":{"name":"v","type":T,"ref":true},"body":[...]}
//	              {"kind":"slice","len":E,"place":P,"elem":...,"body":[...]}
//	optional      protocol.OptionalFunc / OptionalMarshaler: Bool(set); if set, Body runs with Elem bound
//	              to the contained value.
//	              {"kind":"optional","place":P,"elem":{...},"body":[...]}
//	integer_func  protocol.IntegerFunc: a temporary Elem of type Elem.Type (the wire type) is set to the
//	              converted place value, Body runs on it, then the place is set to the converted temporary.
//	              {"kind":"integer_func","place":P,"elem":{"name":"v","type":{"kind":"basic","name":"uint16"},"ref":true},"body":[...]}
//	call          A call of a Function, Enum or Union (Func, Target) whose results are unused; Recv is the
//	              method receiver value, Args follow Function.Params (places for reference parameters).
//	              {"kind":"call","func":"protocol.CommandOriginData","target":"function","args":[P]}
//	var           var Name Type [= Value]
//	              {"kind":"var","name":"categoryType","type":T}
//	assign        Lhs Op Rhs. Op is "=", ":=" (defines the "var" Lhs names, typed), or a compound operator
//	              such as "+=". Several Lhs with one Rhs unpack a tuple. x++ is "+=" 1.
//	              {"kind":"assign","op":"=","lhs":[P],"rhs":[E]}
//	if            if Init; Cond { Then } else { Else }. Else holds a nested "if" for "else if".
//	              {"kind":"if","init":[...],"cond":E,"then":[...],"else":[...]}
//	switch        switch Init; Tag { Cases }. Without Tag each case value is a bool condition. Cases are
//	              tried in order; a case with Default true matches when no other case does. No fallthrough.
//	              {"kind":"switch","tag":E,"cases":[{"values":[E],"body":[...]},{"default":true,"body":[...]}]}
//	type_switch   switch Bind := X.(type). Case Types are variant types (pointer types as written, for
//	              example {"kind":"pointer","elem":{"kind":"named","name":"protocol.X"}}) or null for nil.
//	              {"kind":"type_switch","x":E,"cases":[{"types":[T,null],"body":[...]}]}
//	for           for Init; Cond; Post { Body }
//	range         for Key, Value := range X { Body } (Key/Value absent when not bound)
//	return        return Values...
//	              {"kind":"return","values":[E]}
//	error         A decode/encode failure through the IO. Error is "invalid_value" (Value, Field, Reason:
//	              IO.InvalidValue) or "unknown_enum_option" (Value, Enum: IO.UnknownEnumOption). Field, Reason and
//	              Enum are string expressions (usually "lit"; a reason may be computed). Both the reader and
//	              the writer abort the whole packet at this point.
//	              {"kind":"error","error":"invalid_value","value":E,"field":{"kind":"lit","value":"message",...},"reason":E}
//	              {"kind":"error","error":"unknown_enum_option","value":E,"enum":E}
type Stmt struct {
	Kind string `json:"kind"`

	// io
	Method string `json:"method,omitempty"`
	// io, marshal, slice, optional, integer_func
	Place *Expr `json:"place,omitempty"`
	// io extra arguments, call arguments
	Args []Expr `json:"args,omitempty"`

	// marshal, var
	Type    string `json:"type,omitempty"`
	Dynamic bool   `json:"dynamic,omitempty"`

	// slice
	Prefix string `json:"prefix,omitempty"`
	Len    *Expr  `json:"len,omitempty"`
	// slice, optional, integer_func
	Elem *Param `json:"elem,omitempty"`
	// slice, optional, integer_func, for, range
	Body []Stmt `json:"body,omitempty"`

	// call
	Func   string `json:"func,omitempty"`
	Target string `json:"target,omitempty"`
	Recv   *Expr  `json:"recv,omitempty"`

	// var
	Name    string `json:"name,omitempty"`
	VarType *Type  `json:"var_type,omitempty"`
	// var, error
	Value *Expr `json:"value,omitempty"`

	// assign
	Op  string `json:"op,omitempty"`
	Lhs []Expr `json:"lhs,omitempty"`
	Rhs []Expr `json:"rhs,omitempty"`

	// if, switch, type_switch, for
	Init []Stmt `json:"init,omitempty"`
	// if, for
	Cond *Expr  `json:"cond,omitempty"`
	Then []Stmt `json:"then,omitempty"`
	Else []Stmt `json:"else,omitempty"`
	// for
	Post []Stmt `json:"post,omitempty"`

	// switch
	Tag   *Expr  `json:"tag,omitempty"`
	Cases []Case `json:"cases,omitempty"`
	// type_switch: Bind is the bound name or empty; X is the switched value.
	Bind string `json:"bind,omitempty"`
	// type_switch, range
	X *Expr `json:"x,omitempty"`

	// range
	Key    *Expr `json:"key,omitempty"`
	RValue *Expr `json:"range_value,omitempty"`
	Define bool  `json:"define,omitempty"`

	// return
	Values []Expr `json:"values,omitempty"`

	// error
	Error  string `json:"error,omitempty"`
	Field  *Expr  `json:"field,omitempty"`
	Reason *Expr  `json:"reason,omitempty"`
	Enum   *Expr  `json:"enum,omitempty"`
}

// Case is one switch or type_switch case.
type Case struct {
	// Values are the case expressions (switch).
	Values []Expr `json:"values,omitempty"`
	// Types are the case types (type_switch); a null entry is the nil case.
	Types []*Type `json:"types,omitempty"`
	// Default marks the default case.
	Default bool `json:"default,omitempty"`
	// Body is the case body.
	Body []Stmt `json:"body"`
}

// Expr is one expression. Every expression carries its Go type (Type) except "blank". Kind selects the
// meaning of the other fields:
//
//	var        a parameter, receiver, local or bound element: {"kind":"var","name":"pk","type":T}
//	field      X.Name (implicit pointer dereference): {"kind":"field","x":E,"name":"Type","type":T}
//	index      X[Index]
//	slice_of   X[Low:High] (Low/High absent when omitted)
//	deref      *X of a pointer-typed value (not used for reference bindings, which are never pointers)
//	const      a named constant: {"kind":"const","name":"packet.TextTypeChat","value":"1","type":T}
//	lit        a literal or folded constant expression: {"kind":"lit","value":"256","type":T}
//	nil        the nil value of Type (Type absent for an untyped nil in a comparison)
//	external   a package-level variable of another package: {"kind":"external","name":"nbt.NetworkLittleEndian"}
//	unary      Op X with Op one of "!", "-", "^"
//	binary     X Op Y with Op one of the Go operators ("==", "&&", "+", "<<", "&^", ...)
//	convert    Type(X): {"kind":"convert","x":E,"type":T}
//	call       a call with results: Func, Target ("function", "enum", "union" or "builtin"), Recv for
//	           methods, Args (places for reference parameters). Builtins are Go builtins and standard
//	           library functions named as written: "len", "slices.Contains", "fmt.Sprintf".
//	           {"kind":"call","func":"len","target":"builtin","args":[E],"type":T}
//	composite  a struct value with keyed Fields (absent fields are zero; an optional type literal has no
//	           fields and is the unset Optional): {"kind":"composite","type":T,"fields":[{"name":"set","value":E}]},
//	           or an array or slice value with Elems in order: {"kind":"composite","type":T,"elems":[E,E]}
//	new        a pointer to a fresh struct value (&S{...}), used as a union variant; Type is the pointer
//	           type, Fields as in composite
//	optional_value  X.Value() of a protocol.Optional: the tuple (value, set)
//	option     protocol.Option(X): a set Optional holding X
//	shield_id  IO.ShieldID(): the item network ID of the shield, configured on the IO
//	blank      the blank identifier "_" on the left of an assignment
type Expr struct {
	Kind   string       `json:"kind"`
	Name   string       `json:"name,omitempty"`
	Value  *string      `json:"value,omitempty"`
	Op     string       `json:"op,omitempty"`
	X      *Expr        `json:"x,omitempty"`
	Y      *Expr        `json:"y,omitempty"`
	Index  *Expr        `json:"index,omitempty"`
	Low    *Expr        `json:"low,omitempty"`
	High   *Expr        `json:"high,omitempty"`
	Func   string       `json:"func,omitempty"`
	Target string       `json:"target,omitempty"`
	Recv   *Expr        `json:"recv,omitempty"`
	Args   []Expr       `json:"args,omitempty"`
	Fields []FieldValue `json:"fields,omitempty"`
	Elems  []Expr       `json:"elems,omitempty"`
	Type   *Type        `json:"type,omitempty"`
}

// FieldValue is one keyed field of a composite literal.
type FieldValue struct {
	Name  string `json:"name"`
	Value Expr   `json:"value"`
}
