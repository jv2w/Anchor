//! The protocol IR read from `xtask/data/protocol.json`. The schema is documented in
//! `xtask/go/extract/ir.go`; these types mirror it field for field.

use serde::Deserialize;

/// The root object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ir {
    pub(crate) source: Source,
    pub(crate) io_methods: Vec<IoMethod>,
    pub(crate) packets: Vec<Packet>,
    pub(crate) types: Vec<TypeDef>,
    pub(crate) functions: Vec<Function>,
    pub(crate) enums: Vec<Enum>,
    pub(crate) unions: Vec<Union>,
    pub(crate) const_decls: Vec<ConstDecl>,
}

/// The gophertunnel snapshot.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub(crate) version: String,
    pub(crate) commit: String,
    pub(crate) protocol: i64,
    pub(crate) game_version: String,
}

/// A method of `protocol.IO`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IoMethod {
    pub(crate) name: String,
}

/// A registered packet.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Packet {
    pub(crate) id: u32,
    #[serde(rename = "type")]
    pub(crate) ty: String,
    pub(crate) direction: String,
}

/// A named Go type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeDef {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) underlying: Option<Type>,
    #[serde(default)]
    pub(crate) fields: Vec<Field>,
    pub(crate) marshal: Option<Body>,
}

/// A Marshal body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Body {
    pub(crate) receiver: String,
    pub(crate) stmts: Vec<Stmt>,
}

/// A struct field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Field {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) ty: Type,
    #[serde(default, rename = "embedded")]
    _embedded: bool,
}

/// A structured Go type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Type {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) name: String,
    pub(crate) len: Option<i64>,
    pub(crate) key: Option<Box<Type>>,
    pub(crate) elem: Option<Box<Type>>,
    #[serde(default)]
    pub(crate) elems: Vec<Type>,
}

/// A function or callback parameter.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Param {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) ty: Type,
    #[serde(default, rename = "ref")]
    pub(crate) by_ref: bool,
}

/// A free function or a method other than `Marshal(IO)`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) receiver: Option<Param>,
    pub(crate) io: bool,
    pub(crate) params: Vec<Param>,
    #[serde(default)]
    pub(crate) results: Vec<Type>,
    pub(crate) body: Vec<Stmt>,
}

/// A switch-table conversion function.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Enum {
    pub(crate) name: String,
    #[serde(rename = "io")]
    _io: bool,
    pub(crate) params: Vec<Param>,
    #[serde(default)]
    pub(crate) results: Vec<Type>,
    pub(crate) on: String,
    pub(crate) output: String,
    pub(crate) cases: Vec<EnumCase>,
    #[serde(default)]
    pub(crate) default: Vec<Stmt>,
}

/// One case of an [`Enum`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnumCase {
    pub(crate) values: Vec<Expr>,
    pub(crate) results: Vec<Expr>,
}

/// An interface variant table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Union {
    pub(crate) name: String,
    pub(crate) interface: String,
    pub(crate) id_type: Type,
    pub(crate) key: String,
    pub(crate) results: Vec<Type>,
    pub(crate) entries: Vec<UnionEntry>,
}

/// One row of a [`Union`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnionEntry {
    pub(crate) ids: Vec<Expr>,
    pub(crate) variants: Vec<Option<String>>,
    #[serde(default)]
    pub(crate) extra: Vec<Expr>,
}

/// A const declaration with its exported names.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConstDecl {
    pub(crate) package: String,
    pub(crate) file: String,
    pub(crate) consts: Vec<DeclConst>,
}

/// An exported constant of a const declaration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclConst {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) ty: String,
    pub(crate) basic: String,
    pub(crate) value: String,
    pub(crate) alias: Option<String>,
}

/// One statement.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stmt {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) method: String,
    pub(crate) place: Option<Expr>,
    #[serde(default)]
    pub(crate) args: Vec<Expr>,
    #[serde(default, rename = "type")]
    pub(crate) ty: String,
    #[serde(default)]
    pub(crate) dynamic: bool,
    #[serde(default)]
    pub(crate) prefix: String,
    pub(crate) len: Option<Expr>,
    pub(crate) elem: Option<Param>,
    #[serde(default)]
    pub(crate) body: Vec<Stmt>,
    #[serde(default)]
    pub(crate) func: String,
    #[serde(default)]
    pub(crate) target: String,
    pub(crate) recv: Option<Expr>,
    #[serde(default)]
    pub(crate) name: String,
    pub(crate) var_type: Option<Type>,
    pub(crate) value: Option<Expr>,
    #[serde(default)]
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) lhs: Vec<Expr>,
    #[serde(default)]
    pub(crate) rhs: Vec<Expr>,
    #[serde(default)]
    pub(crate) init: Vec<Stmt>,
    pub(crate) cond: Option<Expr>,
    #[serde(default)]
    pub(crate) then: Vec<Stmt>,
    #[serde(default, rename = "else")]
    pub(crate) els: Vec<Stmt>,
    #[serde(default)]
    pub(crate) post: Vec<Stmt>,
    pub(crate) tag: Option<Expr>,
    #[serde(default)]
    pub(crate) cases: Vec<Case>,
    #[serde(default, rename = "bind")]
    _bind: String,
    pub(crate) x: Option<Expr>,
    pub(crate) key: Option<Expr>,
    pub(crate) range_value: Option<Expr>,
    #[serde(default, rename = "define")]
    _define: bool,
    #[serde(default)]
    pub(crate) values: Vec<Expr>,
    #[serde(default)]
    pub(crate) error: String,
    pub(crate) field: Option<Expr>,
    pub(crate) reason: Option<Expr>,
    #[serde(rename = "enum")]
    pub(crate) enum_name: Option<Expr>,
}

impl Stmt {
    /// The statement lists nested in the statement: its bodies, branches, loop parts and case
    /// bodies.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = &[Stmt]> {
        [&self.body, &self.then, &self.els, &self.init, &self.post]
            .into_iter()
            .map(Vec::as_slice)
            .chain(self.cases.iter().map(|c| c.body.as_slice()))
    }
}

/// One switch or type switch case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Case {
    #[serde(default)]
    pub(crate) values: Vec<Expr>,
    #[serde(default, rename = "types")]
    _types: Vec<Option<Type>>,
    #[serde(default)]
    pub(crate) default: bool,
    pub(crate) body: Vec<Stmt>,
}

/// One expression.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expr {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    #[serde(default)]
    pub(crate) op: String,
    pub(crate) x: Option<Box<Expr>>,
    pub(crate) y: Option<Box<Expr>>,
    pub(crate) index: Option<Box<Expr>>,
    pub(crate) low: Option<Box<Expr>>,
    pub(crate) high: Option<Box<Expr>>,
    #[serde(default)]
    pub(crate) func: String,
    #[serde(default)]
    pub(crate) target: String,
    pub(crate) recv: Option<Box<Expr>>,
    #[serde(default)]
    pub(crate) args: Vec<Expr>,
    #[serde(default)]
    pub(crate) fields: Vec<FieldValue>,
    #[serde(default)]
    pub(crate) elems: Vec<Expr>,
    #[serde(rename = "type")]
    pub(crate) ty: Option<Type>,
}

/// One keyed field of a composite literal.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FieldValue {
    pub(crate) name: String,
    pub(crate) value: Expr,
}
