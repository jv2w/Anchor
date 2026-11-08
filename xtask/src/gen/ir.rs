//! The protocol IR read from `xtask/data/protocol.json`. The schema is documented in
//! `xtask/go/extract/ir.go`; these types mirror it field for field.

use serde::Deserialize;

/// The root object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ir {
    pub(super) source: Source,
    pub(super) io_methods: Vec<IoMethod>,
    pub(super) packets: Vec<Packet>,
    pub(super) types: Vec<TypeDef>,
    pub(super) functions: Vec<Function>,
    pub(super) enums: Vec<Enum>,
    pub(super) unions: Vec<Union>,
    pub(super) const_decls: Vec<ConstDecl>,
}

/// The gophertunnel snapshot.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    pub(super) version: String,
    pub(super) commit: String,
    pub(super) protocol: i64,
    pub(super) game_version: String,
}

/// A method of `protocol.IO`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IoMethod {
    pub(super) name: String,
}

/// A registered packet.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Packet {
    pub(super) id: u32,
    #[serde(rename = "type")]
    pub(super) ty: String,
    pub(super) direction: String,
}

/// A named Go type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TypeDef {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) underlying: Option<Type>,
    #[serde(default)]
    pub(super) fields: Vec<Field>,
    pub(super) marshal: Option<Body>,
}

/// A Marshal body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Body {
    pub(super) receiver: String,
    pub(super) stmts: Vec<Stmt>,
}

/// A struct field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Field {
    pub(super) name: String,
    #[serde(rename = "type")]
    pub(super) ty: Type,
    #[serde(default, rename = "embedded")]
    _embedded: bool,
}

/// A structured Go type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Type {
    pub(super) kind: String,
    #[serde(default)]
    pub(super) name: String,
    pub(super) len: Option<i64>,
    pub(super) key: Option<Box<Type>>,
    pub(super) elem: Option<Box<Type>>,
    #[serde(default)]
    pub(super) elems: Vec<Type>,
}

/// A function or callback parameter.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Param {
    pub(super) name: String,
    #[serde(rename = "type")]
    pub(super) ty: Type,
    #[serde(default, rename = "ref")]
    pub(super) by_ref: bool,
}

/// A free function or a method other than `Marshal(IO)`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Function {
    pub(super) name: String,
    pub(super) receiver: Option<Param>,
    pub(super) io: bool,
    pub(super) params: Vec<Param>,
    #[serde(default)]
    pub(super) results: Vec<Type>,
    pub(super) body: Vec<Stmt>,
}

/// A switch-table conversion function.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Enum {
    pub(super) name: String,
    #[serde(rename = "io")]
    _io: bool,
    pub(super) params: Vec<Param>,
    #[serde(default)]
    pub(super) results: Vec<Type>,
    pub(super) on: String,
    pub(super) output: String,
    pub(super) cases: Vec<EnumCase>,
    #[serde(default)]
    pub(super) default: Vec<Stmt>,
}

/// One case of an [`Enum`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnumCase {
    pub(super) values: Vec<Expr>,
    pub(super) results: Vec<Expr>,
}

/// An interface variant table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Union {
    pub(super) name: String,
    pub(super) interface: String,
    pub(super) id_type: Type,
    pub(super) key: String,
    pub(super) results: Vec<Type>,
    pub(super) entries: Vec<UnionEntry>,
}

/// One row of a [`Union`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UnionEntry {
    pub(super) ids: Vec<Expr>,
    pub(super) variants: Vec<Option<String>>,
    #[serde(default)]
    pub(super) extra: Vec<Expr>,
}

/// A const declaration with its exported names.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConstDecl {
    pub(super) package: String,
    pub(super) file: String,
    pub(super) consts: Vec<DeclConst>,
}

/// An exported constant of a const declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeclConst {
    pub(super) name: String,
    #[serde(rename = "type")]
    pub(super) ty: String,
    pub(super) basic: String,
    pub(super) value: String,
    pub(super) alias: Option<String>,
}

/// One statement.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stmt {
    pub(super) kind: String,
    #[serde(default)]
    pub(super) method: String,
    pub(super) place: Option<Expr>,
    #[serde(default)]
    pub(super) args: Vec<Expr>,
    #[serde(default, rename = "type")]
    pub(super) ty: String,
    #[serde(default)]
    pub(super) dynamic: bool,
    #[serde(default)]
    pub(super) prefix: String,
    pub(super) len: Option<Expr>,
    pub(super) elem: Option<Param>,
    #[serde(default)]
    pub(super) body: Vec<Stmt>,
    #[serde(default)]
    pub(super) func: String,
    #[serde(default)]
    pub(super) target: String,
    pub(super) recv: Option<Expr>,
    #[serde(default)]
    pub(super) name: String,
    pub(super) var_type: Option<Type>,
    pub(super) value: Option<Expr>,
    #[serde(default)]
    pub(super) op: String,
    #[serde(default)]
    pub(super) lhs: Vec<Expr>,
    #[serde(default)]
    pub(super) rhs: Vec<Expr>,
    #[serde(default)]
    pub(super) init: Vec<Stmt>,
    pub(super) cond: Option<Expr>,
    #[serde(default)]
    pub(super) then: Vec<Stmt>,
    #[serde(default, rename = "else")]
    pub(super) els: Vec<Stmt>,
    #[serde(default)]
    pub(super) post: Vec<Stmt>,
    pub(super) tag: Option<Expr>,
    #[serde(default)]
    pub(super) cases: Vec<Case>,
    #[serde(default, rename = "bind")]
    _bind: String,
    pub(super) x: Option<Expr>,
    pub(super) key: Option<Expr>,
    pub(super) range_value: Option<Expr>,
    #[serde(default, rename = "define")]
    _define: bool,
    #[serde(default)]
    pub(super) values: Vec<Expr>,
    #[serde(default)]
    pub(super) error: String,
    pub(super) field: Option<Expr>,
    pub(super) reason: Option<Expr>,
    #[serde(rename = "enum")]
    pub(super) enum_name: Option<Expr>,
}

impl Stmt {
    /// The statement lists nested in the statement: its bodies, branches, loop parts and case
    /// bodies.
    pub(super) fn blocks(&self) -> impl Iterator<Item = &[Stmt]> {
        [&self.body, &self.then, &self.els, &self.init, &self.post]
            .into_iter()
            .map(Vec::as_slice)
            .chain(self.cases.iter().map(|c| c.body.as_slice()))
    }
}

/// One switch or type switch case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Case {
    #[serde(default)]
    pub(super) values: Vec<Expr>,
    #[serde(default, rename = "types")]
    _types: Vec<Option<Type>>,
    #[serde(default)]
    pub(super) default: bool,
    pub(super) body: Vec<Stmt>,
}

/// One expression.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expr {
    pub(super) kind: String,
    #[serde(default)]
    pub(super) name: String,
    pub(super) value: Option<String>,
    #[serde(default)]
    pub(super) op: String,
    pub(super) x: Option<Box<Expr>>,
    pub(super) y: Option<Box<Expr>>,
    pub(super) index: Option<Box<Expr>>,
    pub(super) low: Option<Box<Expr>>,
    pub(super) high: Option<Box<Expr>>,
    #[serde(default)]
    pub(super) func: String,
    #[serde(default)]
    pub(super) target: String,
    pub(super) recv: Option<Box<Expr>>,
    #[serde(default)]
    pub(super) args: Vec<Expr>,
    #[serde(default)]
    pub(super) fields: Vec<FieldValue>,
    #[serde(default)]
    pub(super) elems: Vec<Expr>,
    #[serde(rename = "type")]
    pub(super) ty: Option<Type>,
}

/// One keyed field of a composite literal.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FieldValue {
    pub(super) name: String,
    pub(super) value: Expr,
}
