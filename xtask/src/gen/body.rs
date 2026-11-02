//! Translation of Go statement bodies into Rust. A Go `Marshal(io)` body runs in both
//! directions; each body is lowered once for decoding and once for encoding into a small
//! statement tree, dead stores to locals are removed, and the tree is printed.
//!
//! Directions differ only where the Go code does: a reader fills the places it is given, a
//! writer reads them. A Go writer may also assign the value being written (an enum string
//! converted back, a normalised field); such an assignment is dropped when nothing reads the
//! place afterwards, and otherwise goes to a local copy of the field (a shadow). List and
//! optional element bodies and loops are printed eagerly as closures, blocks or loops, which
//! may read but never assign the locals around them.

use std::collections::{BTreeMap, BTreeSet};

use super::ir::{Expr, Function, Param, Stmt, Type};
use super::model::{self, Model, Module, Pass, Read};
use super::names;

/// The prefix of the helper entry that marks a decode limit (`Model::limits`) as applied.
pub(crate) const LIMIT_USED: &str = "limit ";

/// The direction a body is lowered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    Decode,
    Encode,
}

type Vars = BTreeSet<usize>;

/// A Rust expression and the tracked locals it reads.
#[derive(Debug, Clone, Default)]
struct E {
    code: String,
    reads: Vars,
    /// Whether evaluating it may fail or does IO.
    effects: bool,
    /// Locals it borrows mutably.
    muts: Vars,
}

impl E {
    fn new(code: String) -> Self {
        Self {
            code,
            ..Self::default()
        }
    }

    fn with_code(mut self, code: String) -> Self {
        self.code = code;
        self
    }

    fn with(mut self, other: &E) -> Self {
        self.reads.extend(&other.reads);
        self.muts.extend(&other.muts);
        self.effects |= other.effects;
        self
    }

    fn effect(mut self) -> Self {
        self.effects = true;
        self
    }
}

/// How a translated value is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    /// A `Copy` value.
    Copy,
    /// A reference (`&T`, or `&str` for strings).
    Ref,
    /// An owned temporary.
    Owned,
    /// A non-`Copy` place reached through a field or index (cloned to own).
    Place,
    /// A non-`Copy` local (moved to own).
    Local,
}

#[derive(Debug, Clone)]
struct Val {
    e: E,
    ty: Type,
    form: Form,
    /// The Rust primitive of a number or bool, `usize` for Go `int` values from lengths and
    /// indices, `None` for untyped constants and other types.
    prim: Option<&'static str>,
}

/// Whether a local holds its values or references to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holding {
    /// The local owns its values.
    Owned,
    /// The local holds references (an encoder's `Option<&T>`).
    Borrowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BKind {
    /// A local or path holding the value.
    Owned,
    /// A shared reference to the value.
    Ref,
    /// A mutable reference to the value (`code` is `*name`).
    RefMut,
    /// A non-nil Go `error`: its `&'static str` message.
    Message,
}

#[derive(Debug, Clone)]
struct Binding {
    code: String,
    ty: Type,
    kind: BKind,
    var: Option<usize>,
    /// Whether writes to it reach data outside the body (receiver, parameters, elements).
    external: bool,
    /// Whether it is a local holding borrowed values (an encoder's `Option<&T>`).
    borrowed: bool,
}

#[derive(Debug, Clone)]
struct Var {
    name: String,
    ty: Option<String>,
}

#[derive(Debug, Clone)]
enum Cond {
    Plain {
        e: E,
        neg: Option<String>,
    },
    Let {
        var: usize,
        expr: E,
        extra: Option<E>,
    },
}

#[derive(Debug, Clone)]
enum S {
    /// `let pat [= init];` `default_of` is the type and field count of a `T::default()`
    /// initializer that later field stores may fold into.
    Let {
        vars: Vec<Option<usize>>,
        init: Option<E>,
        default_of: Option<(String, usize)>,
    },
    Set {
        var: usize,
        rhs: E,
    },
    Store {
        place: String,
        root: Option<usize>,
        field: Option<String>,
        reads: Vars,
        rhs: E,
    },
    Expr(E),
    If {
        cond: Cond,
        then: Vec<S>,
        els: Vec<S>,
    },
    Match {
        scrut: E,
        arms: Vec<(Vec<String>, Vec<S>)>,
    },
    Return(Option<E>),
    /// `return Err(..)`.
    Fail(E),
}

/// Where printed statements go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintMode {
    /// A function body: the last `Return` is the tail expression.
    Fn,
    /// A closure returning `Result`: the last `Return` is `Ok(value)`.
    Closure,
    /// A block giving a value: the last `Return` is the value.
    Block,
    /// A loop body: every `Return` returns from the function.
    Loop,
}

/// What an expression is to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The index of an index expression.
    Index,
    /// Anything else.
    Other,
}

/// Whether printed statements end the body they are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The last of them is the last statement of the body.
    EndsBody,
    /// More statements follow them.
    Inside,
}

/// What a function looks like to its callers in one direction.
#[derive(Debug, Clone, Default)]
pub(crate) struct Summary {
    /// Whether it returns `Result`.
    pub(crate) fallible: bool,
    /// Whether each parameter (receiver first) is kept; unused ones are dropped.
    pub(crate) used: Vec<bool>,
    /// Encode: `Copy` reference parameters it assigns, returned after the call.
    pub(crate) in_out: Vec<usize>,
    /// Encode: other reference parameters it assigns (the assignment is dropped).
    pub(crate) written: Vec<usize>,
    /// The Rust result type, without `Result`.
    pub(crate) result: String,
}

/// Function summaries by Go name and whether the direction is decoding.
pub(crate) type Summaries = BTreeMap<(String, bool), Summary>;

/// The result of lowering a body.
#[derive(Debug)]
pub(crate) struct Lowered {
    /// The parameter list (functions only).
    pub(crate) params: String,
    /// The body statements.
    pub(crate) body: String,
    /// Whether the body may fail.
    pub(crate) fallible: bool,
    /// Helper functions it uses.
    pub(crate) helpers: BTreeSet<String>,
}

#[derive(Debug, Clone)]
enum Ret {
    /// `pk` (a decoded receiver).
    Receiver(usize),
    /// `()`.
    Unit,
    /// A function: its result values (from `return`) plus in-out parameters.
    Values {
        results: Vec<String>,
        in_out: Vec<usize>,
    },
}

/// A lowering in progress.
struct Lower<'m, 'a> {
    m: &'m Model<'a>,
    sums: &'m Summaries,
    dir: Dir,
    module: Module,
    vars: Vec<Var>,
    scopes: Vec<BTreeMap<String, Binding>>,
    /// Encode: shadow locals of written external places, by Go path.
    shadows: BTreeMap<String, usize>,
    ret: Ret,
    helpers: BTreeSet<String>,
    fallible: bool,
    /// Go names bound to `usize` values (range keys).
    usize_names: BTreeSet<String>,
    /// The limit key of the locals that list and optional bodies read into or write from,
    /// innermost last (`None` for a local of a place without a key).
    limit_vars: Vec<(String, Option<String>)>,
}

impl<'m, 'a> Lower<'m, 'a> {
    fn new(m: &'m Model<'a>, sums: &'m Summaries, dir: Dir, module: Module) -> Self {
        Self {
            m,
            sums,
            dir,
            module,
            vars: Vec::new(),
            scopes: vec![BTreeMap::new()],
            shadows: BTreeMap::new(),
            ret: Ret::Unit,
            helpers: BTreeSet::new(),
            fallible: false,
            usize_names: BTreeSet::new(),
            limit_vars: Vec::new(),
        }
    }

    fn r(&self) -> &'static str {
        match self.dir {
            Dir::Decode => "r",
            Dir::Encode => "w",
        }
    }

    fn rust_type(&self, t: &Type) -> Result<String, String> {
        self.m.rust_type(t, self.module)
    }

    /// A function result type: `sum` decides strings (`&str`, `&'static str` or `String`).
    fn result_type(&self, t: &Type, sum: &str) -> Result<String, String> {
        if is_string(t) {
            return Ok(if sum.contains("String") {
                "String".to_owned()
            } else {
                "&str".to_owned()
            });
        }
        if t.kind == "error" {
            return Ok("Option<&'static str>".to_owned());
        }
        self.rust_type(t)
    }

    /// The type of a borrowed local holding a value of `t`.
    fn borrowed_type(&self, t: &Type) -> Result<String, String> {
        if self.m.is_copy(t) {
            return self.rust_type(t);
        }
        Ok(match t.kind.as_str() {
            "basic" if is_string(t) => "&str".to_owned(),
            "optional" => format!("Option<{}>", self.borrowed_type(model::elem(t)?)?),
            "array" => format!(
                "[{}; {}]",
                self.borrowed_type(model::elem(t)?)?,
                t.len.unwrap_or(0)
            ),
            "error" => "Option<&'static str>".to_owned(),
            _ => format!("&{}", self.rust_type(t)?),
        })
    }

    fn name(&self, id: usize) -> String {
        self.vars
            .get(id)
            .map(|v| v.name.clone())
            .unwrap_or_default()
    }

    fn new_var(&mut self, go: &str, ty: Option<String>) -> Result<usize, String> {
        let name = param_name(go)?;
        self.vars.push(Var { name, ty });
        Ok(self.vars.len() - 1)
    }

    fn bind(&mut self, go: &str, b: Binding) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(go.to_owned(), b);
        }
    }

    fn lookup(&self, go: &str) -> Result<Binding, String> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.get(go))
            .cloned()
            .ok_or_else(|| format!("unknown variable {go}"))
    }

    /// How a local holds its values in this direction: encoders borrow, decoders own.
    fn direction_holding(&self) -> Holding {
        match self.dir {
            Dir::Decode => Holding::Owned,
            Dir::Encode => Holding::Borrowed,
        }
    }

    /// Declares a tracked local bound to a Go name, with its type written out unless it is a
    /// tuple; `Borrowed` locals hold references.
    fn declare(&mut self, go: &str, t: &Type, holding: Holding) -> Result<usize, String> {
        let ty = match (t.kind.as_str(), holding) {
            ("tuple", _) => None,
            (_, Holding::Borrowed) => Some(self.borrowed_type(t)?),
            (_, Holding::Owned) => Some(self.rust_type(t)?),
        };
        self.declare_typed(go, t, ty, holding)
    }

    /// [`Self::declare`] with the type left to inference.
    fn declare_unannotated(
        &mut self,
        go: &str,
        t: &Type,
        holding: Holding,
    ) -> Result<usize, String> {
        self.declare_typed(go, t, None, holding)
    }

    /// Declares a tracked local whose written type is `ty`.
    fn declare_typed(
        &mut self,
        go: &str,
        t: &Type,
        ty: Option<String>,
        holding: Holding,
    ) -> Result<usize, String> {
        let borrowed = holding == Holding::Borrowed;
        let reference = borrowed && self.borrowed_type(t)?.starts_with('&');
        let id = self.new_var(go, ty)?;
        let kind = if reference { BKind::Ref } else { BKind::Owned };
        self.bind(
            go,
            Binding {
                code: self.name(id),
                ty: t.clone(),
                kind,
                var: Some(id),
                external: false,
                borrowed,
            },
        );
        Ok(id)
    }

    fn q(&mut self) -> &'static str {
        self.fallible = true;
        "?"
    }

    /// Encode: declares shadow locals for external places the body assigns and reads after.
    fn add_shadows(&mut self, stmts: &[Stmt], out: &mut Vec<S>) -> Result<(), String> {
        let mut locals = BTreeSet::new();
        collect_locals(stmts, &mut locals);
        let mut written = Vec::new();
        let mut needs = BTreeSet::new();
        self.flow(stmts, &locals, &mut written, &mut needs);
        for path in needs {
            let parts: Vec<&str> = path.split('.').collect();
            let root = self.lookup(parts.first().copied().unwrap_or_default())?;
            if root.var.is_some() {
                continue;
            }
            let [_, field] = parts.as_slice() else {
                return Err(format!("the writer assigns {path} and reads it afterwards"));
            };
            let (name, t) = self.field_of(&root.ty, field)?;
            if !self.m.is_copy(&t) {
                return Err(format!(
                    "the writer assigns {path} and reads it afterwards; it is not Copy"
                ));
            }
            let id = self.new_var(field, Some(self.rust_type(&t)?))?;
            self.shadows.insert(path.clone(), id);
            out.push(S::Let {
                vars: vec![Some(id)],
                init: Some(E::new(format!("{}.{name}", root.code))),
                default_of: None,
            });
        }
        Ok(())
    }

    /// Follows encode-direction writes of external places through the control flow and
    /// records the written places read on a later path.
    fn flow(
        &self,
        stmts: &[Stmt],
        locals: &BTreeSet<String>,
        written: &mut Vec<String>,
        needs: &mut BTreeSet<String>,
    ) {
        let note = |events: Vec<(String, bool)>,
                    written: &mut Vec<String>,
                    needs: &mut BTreeSet<String>| {
            for (path, write) in events {
                if locals.contains(path.split('.').next().unwrap_or_default()) {
                    continue;
                }
                if write {
                    written.push(path);
                } else {
                    for w in written.iter() {
                        if overlaps(w, &path) {
                            needs.insert(w.clone());
                        }
                    }
                }
            }
        };
        for s in stmts {
            match s.kind.as_str() {
                "if" | "switch" => {
                    let mut head = Vec::new();
                    self.events(&s.init, &mut head);
                    for e in [&s.cond, &s.tag].into_iter().flatten() {
                        expr_paths(e, &mut head);
                    }
                    for c in &s.cases {
                        for v in &c.values {
                            expr_paths(v, &mut head);
                        }
                    }
                    note(head, written, needs);
                    let mut all = written.clone();
                    let branches: Vec<&Vec<Stmt>> = if s.kind == "if" {
                        vec![&s.then, &s.els]
                    } else {
                        s.cases.iter().map(|c| &c.body).collect()
                    };
                    for b in branches {
                        let mut w = written.clone();
                        self.flow(b, locals, &mut w, needs);
                        for x in w {
                            if !all.contains(&x) {
                                all.push(x);
                            }
                        }
                    }
                    *written = all;
                }
                "for" | "range" => {
                    let mut head = Vec::new();
                    for e in [&s.cond, &s.x].into_iter().flatten() {
                        expr_paths(e, &mut head);
                    }
                    note(head, written, needs);
                    for _ in 0..2 {
                        self.flow(&s.body, locals, written, needs);
                    }
                }
                _ => {
                    let mut ev = Vec::new();
                    self.events(std::slice::from_ref(s), &mut ev);
                    note(ev, written, needs);
                }
            }
        }
    }

    /// Encode-direction reads (`false`) and writes (`true`) of places, in order.
    fn events(&self, stmts: &[Stmt], out: &mut Vec<(String, bool)>) {
        for s in stmts {
            match s.kind.as_str() {
                "call" => {
                    if let Some(r) = &s.recv {
                        expr_paths(r, out);
                    }
                    let writes = self.call_writes(&s.func, &s.target);
                    for (i, a) in s.args.iter().enumerate() {
                        expr_paths(a, out);
                        if writes.contains(&i)
                            && let Some(p) = place_path(a)
                        {
                            out.push((p, true));
                        }
                    }
                }
                "assign" => {
                    for r in &s.rhs {
                        expr_paths(r, out);
                    }
                    for l in &s.lhs {
                        if s.op != "=" && s.op != ":=" {
                            expr_paths(l, out);
                        }
                        match place_path(l) {
                            Some(p) => out.push((p, true)),
                            None => expr_paths(l, out),
                        }
                    }
                }
                _ => {
                    for e in [
                        &s.place,
                        &s.len,
                        &s.value,
                        &s.cond,
                        &s.tag,
                        &s.x,
                        &s.field,
                        &s.reason,
                        &s.enum_name,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        expr_paths(e, out);
                    }
                    for v in s.args.iter().chain(&s.values) {
                        expr_paths(v, out);
                    }
                    for list in [&s.init, &s.then, &s.els, &s.body, &s.post] {
                        self.events(list, out);
                    }
                    for c in &s.cases {
                        for v in &c.values {
                            expr_paths(v, out);
                        }
                        self.events(&c.body, out);
                    }
                }
            }
        }
    }

    /// The argument indices a call statement assigns when encoding.
    fn call_writes(&self, func: &str, target: &str) -> Vec<usize> {
        match target {
            "enum" => self
                .m
                .enums
                .get(func)
                .and_then(|e| e.params.iter().position(|p| p.name == e.output))
                .into_iter()
                .collect(),
            "function" => {
                let shift = usize::from(
                    self.m
                        .functions
                        .get(func)
                        .is_some_and(|f| f.receiver.is_some()),
                );
                self.sums
                    .get(&(func.to_owned(), false))
                    .map_or_else(Vec::new, |s| {
                        s.in_out
                            .iter()
                            .chain(&s.written)
                            .filter_map(|i| i.checked_sub(shift))
                            .collect()
                    })
            }
            _ => Vec::new(),
        }
    }

    fn stmts(&mut self, stmts: &[Stmt], out: &mut Vec<S>) -> Result<(), String> {
        for s in stmts {
            self.stmt(s, out)?;
            if ends(out) {
                // The rest of the block is unreachable.
                break;
            }
        }
        Ok(())
    }

    fn scoped(&mut self, stmts: &[Stmt]) -> Result<Vec<S>, String> {
        self.scopes.push(BTreeMap::new());
        let mut out = Vec::new();
        let result = self.stmts(stmts, &mut out);
        self.scopes.pop();
        result.map(|()| out)
    }

    fn stmt(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        match s.kind.as_str() {
            "io" => self.io(s, out),
            "marshal" => self.marshal_stmt(s, out),
            "slice" => self.slice(s, out),
            "optional" => self.optional(s, out),
            "integer_func" => self.integer_func(s, out),
            "call" => self.call_stmt(s, out),
            "var" => {
                let t = s.var_type.as_ref().ok_or("var without type")?;
                let holding = self.direction_holding();
                let init = match &s.value {
                    Some(v) => {
                        let v = self.expr(v)?;
                        self.local_value(v, holding)?
                    }
                    None => self.zero(t)?,
                };
                let id = self.declare(&s.name, t, holding)?;
                out.push(S::Let {
                    vars: vec![Some(id)],
                    init: Some(init),
                    default_of: None,
                });
                Ok(())
            }
            "assign" => self.assign(s, out),
            "if" => {
                self.scopes.push(BTreeMap::new());
                let result = self.if_stmt(s);
                self.scopes.pop();
                out.push(result?);
                Ok(())
            }
            "switch" => self.switch(s, out),
            "for" => self.for_stmt(s, out),
            "range" => self.range(s, out),
            "return" => {
                let st = self.return_stmt(&s.values)?;
                out.push(st);
                Ok(())
            }
            "error" => {
                let st = self.error(s)?;
                out.push(st);
                Ok(())
            }
            other => Err(format!("unsupported statement {other}")),
        }
    }

    fn return_stmt(&mut self, values: &[Expr]) -> Result<S, String> {
        match self.ret.clone() {
            Ret::Receiver(id) => {
                let mut e = E::new(self.name(id));
                e.reads.insert(id);
                Ok(S::Return(Some(e)))
            }
            Ret::Unit => Ok(S::Return(None)),
            Ret::Values { results, in_out } => {
                let mut parts = Vec::new();
                let mut e = E::default();
                for (v, t) in values.iter().zip(&results) {
                    let v = self.expr(v)?;
                    let v = if t == "String" {
                        Self::owned_value(v)
                    } else {
                        self.return_value(v)?
                    };
                    e = e.with(&v);
                    parts.push(v.code);
                }
                for id in in_out {
                    parts.push(self.name(id));
                    e.reads.insert(id);
                }
                e.code = match parts.len() {
                    0 => return Ok(S::Return(None)),
                    1 => parts.concat(),
                    _ => format!("({})", parts.join(", ")),
                };
                Ok(S::Return(Some(e)))
            }
        }
    }

    fn error(&mut self, s: &Stmt) -> Result<S, String> {
        self.fallible = true;
        let mut reads = Vars::new();
        let code = match s.error.as_str() {
            "invalid_value" => {
                let field = self.static_str(s.field.as_ref().ok_or("error without field")?)?;
                let reason = self.static_str(s.reason.as_ref().ok_or("error without reason")?)?;
                reads.extend(&field.reads);
                reads.extend(&reason.reads);
                format!(
                    "Error::InvalidValue {{ field: {}, reason: {} }}",
                    field.code, reason.code
                )
            }
            "unknown_enum_option" => {
                let name = self.static_str(s.enum_name.as_ref().ok_or("error without enum")?)?;
                let value = self.expr(s.value.as_ref().ok_or("error without value")?)?;
                let value = self.i64_value(value)?;
                reads.extend(&value.reads);
                format!(
                    "Error::UnknownEnum {{ name: {}, value: {} }}",
                    name.code, value.code
                )
            }
            other => return Err(format!("unsupported error {other}")),
        };
        let mut e = E::new(code);
        e.reads = reads;
        Ok(S::Fail(e))
    }

    /// A `&'static str` expression: a string constant, or the message of a non-nil error.
    fn static_str(&mut self, e: &Expr) -> Result<E, String> {
        match e.kind.as_str() {
            "lit" | "const" => Ok(E::new(str_lit(e.value.as_deref().unwrap_or_default()))),
            "call" if e.func == "error.Error" => {
                let recv = self.expr(e.recv.as_deref().ok_or("Error without receiver")?)?;
                Ok(recv.e)
            }
            _ => Err(format!("unsupported error text {e:?}")),
        }
    }

    fn io_args(s: &Stmt) -> Result<Vec<String>, String> {
        let mut args = Vec::new();
        for a in &s.args {
            args.push(match a.kind.as_str() {
                "external" => match a.name.as_str() {
                    "nbt.NetworkLittleEndian" => "nbt::Encoding::NetworkLittleEndian".to_owned(),
                    "nbt.LittleEndian" => "nbt::Encoding::LittleEndian".to_owned(),
                    other => return Err(format!("unsupported IO argument {other}")),
                },
                "lit" | "const" => a.value.clone().ok_or("constant without value")?,
                _ => return Err(format!("unsupported IO argument {a:?}")),
            });
        }
        Ok(args)
    }

    /// Decode: `r.method(args)` of an IO statement, returning `Result`.
    fn io_read_call(&mut self, s: &Stmt) -> Result<String, String> {
        let spec =
            model::io_spec(&s.method).ok_or_else(|| format!("unknown IO method {}", s.method))?;
        if spec.read != Read::Returns {
            return Err(format!("{} does not return a value", s.method));
        }
        self.fallible = true;
        let args = Self::io_args(s)?;
        if let Some(max) = self.limit(s.place.as_ref()) {
            if !matches!(spec.rust, "string" | "byte_slice") || !args.is_empty() {
                return Err(format!(
                    "{} has no reader that takes a field limit",
                    s.method
                ));
            }
            return Ok(format!(
                "r.{}_max({})",
                spec.rust,
                int_lit(&max.to_string())
            ));
        }
        Ok(format!("r.{}({})", spec.rust, args.join(", ")))
    }

    /// The decode limit key of a place: `<Go type>.<field>` for a field, or the key a list or
    /// optional body gave the local it reads into.
    fn limit_key(&self, place: &Expr) -> Option<String> {
        match place.kind.as_str() {
            "field" => {
                let ty = place.x.as_deref()?.ty.as_ref()?;
                (ty.kind == "named").then(|| format!("{}.{}", ty.name, place.name))
            }
            "var" => self
                .limit_vars
                .iter()
                .rev()
                .find(|(name, _)| *name == place.name)
                .and_then(|(_, key)| key.clone()),
            _ => None,
        }
    }

    /// The limit of the string or list read into or written from a place: the decoder refuses
    /// what is over it, and the encoder does not write it, so a packet the client would refuse
    /// is never sent, whoever built it. Decode marks it as applied.
    fn limit(&mut self, place: Option<&Expr>) -> Option<usize> {
        let key = self.limit_key(place?)?;
        let max = *self.m.limits.get(&key)?;
        if self.dir == Dir::Decode {
            self.helpers.insert(format!("{LIMIT_USED}{key}"));
        }
        Some(max)
    }

    fn io(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let spec =
            model::io_spec(&s.method).ok_or_else(|| format!("unknown IO method {}", s.method))?;
        let place = s.place.as_ref().ok_or("io without place")?;
        match self.dir {
            Dir::Decode if spec.read == Read::Checks => {
                let v = self.expr(place)?;
                let arg = Self::ref_arg(&v);
                let q = self.q();
                out.push(S::Expr(
                    arg.clone()
                        .with_code(format!("r.{}({}){q}", spec.rust, arg.code))
                        .effect(),
                ));
                Ok(())
            }
            Dir::Decode => {
                let call = self.io_read_call(s)?;
                self.store(place, E::new(format!("{call}?")).effect(), out)
            }
            Dir::Encode => {
                let v = self.expr(place)?;
                let arg = match spec.pass {
                    Pass::Value => self.copy_value(v)?,
                    Pass::Ref => Self::ref_arg(&v),
                };
                let mut args = vec![arg.code.clone()];
                args.extend(Self::io_args(s)?);
                let (method, q) = match self.limit(Some(place)) {
                    Some(max) => {
                        if !matches!(spec.rust, "string" | "byte_slice") || args.len() != 1 {
                            return Err(format!(
                                "{} has no writer that takes a field limit",
                                s.method
                            ));
                        }
                        args.push(int_lit(&max.to_string()));
                        (format!("{}_max", spec.rust), self.q())
                    }
                    None => (
                        spec.rust.to_owned(),
                        if spec.write_fails { self.q() } else { "" },
                    ),
                };
                out.push(S::Expr(
                    arg.with_code(format!("w.{method}({}){q}", args.join(", ")))
                        .effect(),
                ));
                Ok(())
            }
        }
    }

    fn marshal_stmt(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let place = s.place.as_ref().ok_or("marshal without place")?;
        if s.dynamic {
            let v = self.expr(place)?;
            let q = self.q();
            let recv = method_recv(&v.e.code);
            return match self.dir {
                Dir::Decode => {
                    let e =
                        v.e.clone()
                            .with_code(format!(
                                "std::mem::take(&mut {}).decode_variant(r){q}",
                                v.e.code
                            ))
                            .effect();
                    self.store(place, e, out)
                }
                Dir::Encode => {
                    out.push(S::Expr(
                        v.e.clone()
                            .with_code(format!("{recv}.encode_variant(w){q}"))
                            .effect(),
                    ));
                    Ok(())
                }
            };
        }
        if !self.m.has_codec(&s.ty) {
            return Err(format!("marshal of {}, which has no Codec", s.ty));
        }
        match self.dir {
            Dir::Decode => {
                let call = self.marshal_call(&s.ty, Some(place))?;
                self.store(place, E::new(format!("{call}?")).effect(), out)
            }
            Dir::Encode => {
                let v = self.expr(place)?;
                let q = self.q();
                let recv = method_recv(&v.e.code);
                out.push(S::Expr(
                    v.e.clone()
                        .with_code(format!("{recv}.encode(w){q}"))
                        .effect(),
                ));
                Ok(())
            }
        }
    }

    /// Decode: `T::decode(r)`, or `<Arc<T>>::decode(r)` into a shared field.
    fn marshal_call(&mut self, go: &str, place: Option<&Expr>) -> Result<String, String> {
        self.fallible = true;
        let name = self.m.named(go, self.module)?;
        let shared = place.is_some_and(|p| {
            p.kind == "field"
                && p.x
                    .as_deref()
                    .and_then(|x| x.ty.as_ref())
                    .is_some_and(|t| t.kind == "named" && model::is_shared(&t.name, &p.name))
        });
        let name = if shared { format!("Arc<{name}>") } else { name };
        Ok(format!("{}::decode(r)", type_path(&name)))
    }

    /// Decode: the value a single-statement element body gives its element: `(call, true)` for
    /// an expression returning `Result`, `(value, false)` for a plain value.
    fn elem_value(&mut self, body: &[Stmt], elem: &Param) -> Result<Option<(E, bool)>, String> {
        let [s] = body else { return Ok(None) };
        let on_elem = s
            .place
            .as_ref()
            .is_some_and(|p| p.kind == "var" && p.name == elem.name);
        if !on_elem {
            return Ok(None);
        }
        Ok(match s.kind.as_str() {
            "io" if model::io_spec(&s.method).is_some_and(|sp| sp.read == Read::Returns) => {
                Some((E::new(self.io_read_call(s)?), true))
            }
            "marshal" if !s.dynamic && self.m.has_codec(&s.ty) => {
                Some((E::new(self.marshal_call(&s.ty, None)?), true))
            }
            "slice" => Some((self.list_call(s)?, true)),
            "optional" => Some((self.option_value(s)?, false)),
            _ => None,
        })
    }

    /// Decode: `r.list_*(F)` of a slice statement, returning `Result`; `r.list_*_max(N, F)`
    /// when the field holds at most `N` elements.
    fn list_call(&mut self, s: &Stmt) -> Result<E, String> {
        let elem = s.elem.as_ref().ok_or("slice without element")?;
        let max = self.limit(s.place.as_ref());
        let key = s.place.as_ref().and_then(|p| self.limit_key(p));
        self.limit_vars
            .push((elem.name.clone(), key.map(|k| format!("{k}[]"))));
        let f = self.elem_fn(&s.body, elem);
        self.limit_vars.pop();
        let f = f?;
        let (suffix, max) = match max {
            Some(max) => ("_max", format!("{}, ", int_lit(&max.to_string()))),
            None => ("", String::new()),
        };
        Ok(match s.prefix.as_str() {
            "varuint32" | "uint8" | "uint32" => f
                .clone()
                .with_code(format!("r.list_{}{suffix}({max}{})", s.prefix, f.code)),
            "" => {
                let len = self.expr(s.len.as_ref().ok_or("slice without length")?)?;
                let len = self.u64_value(len)?;
                f.clone()
                    .with(&len)
                    .with_code(format!("r.list{suffix}({}, {max}{})", len.code, f.code))
            }
            other => return Err(format!("unsupported slice prefix {other}")),
        })
    }

    /// Decode: the element reader passed to a list read.
    fn elem_fn(&mut self, body: &[Stmt], elem: &Param) -> Result<E, String> {
        self.fallible = true;
        if let Some((value, call)) = self.elem_value(body, elem)? {
            if !call {
                return Ok(value.clone().with_code(format!("|r| Ok({})", value.code)));
            }
            let code = &value.code;
            if let Some(path) = code.strip_suffix("::decode(r)") {
                return Ok(value.clone().with_code(format!("{path}::decode")));
            }
            if let Some(method) = code.strip_prefix("r.").and_then(|m| m.strip_suffix("()"))
                && !method.contains('(')
            {
                return Ok(value.clone().with_code(format!("Reader::{method}")));
            }
            return Ok(value.clone().with_code(format!("|r| {code}")));
        }
        let (stmts, first) = self.elem_stmts(body, elem)?;
        let (code, reads) = self.print_nested(stmts, first, PrintMode::Closure)?;
        let mut e = E::new(format!("|r| {{ {code} }}"));
        e.reads = reads;
        Ok(e)
    }

    /// Decode: statements building one element in a fresh default local, ending in a return of
    /// it; and the first local id of the nested body.
    fn elem_stmts(&mut self, body: &[Stmt], elem: &Param) -> Result<(Vec<S>, usize), String> {
        if contains_return(body) {
            return Err("return inside an element body".to_owned());
        }
        self.scopes.push(BTreeMap::new());
        let rust = self.rust_type(&elem.ty)?;
        let first = self.vars.len();
        let id = self.new_var(&elem.name, None)?;
        self.bind(
            &elem.name,
            Binding {
                code: self.name(id),
                ty: elem.ty.clone(),
                kind: BKind::Owned,
                var: Some(id),
                external: false,
                borrowed: false,
            },
        );
        let fields = if elem.ty.kind == "named" {
            self.m
                .struct_def(&elem.ty.name)
                .map_or(usize::MAX, |d| d.fields.len())
        } else {
            usize::MAX
        };
        let path = type_path(&rust);
        let mut out = vec![S::Let {
            vars: vec![Some(id)],
            init: Some(E::new(format!("{path}::default()"))),
            default_of: Some((path, fields)),
        }];
        let result = self.stmts(body, &mut out);
        self.scopes.pop();
        result?;
        let mut tail = E::new(self.name(id));
        tail.reads.insert(id);
        out.push(S::Return(Some(tail)));
        Ok((out, first))
    }

    /// Cleans and prints a nested body; returns its code and the outer locals it reads. A body
    /// that assigns a local outside it is an error.
    fn print_nested(
        &mut self,
        mut stmts: Vec<S>,
        first: usize,
        mode: PrintMode,
    ) -> Result<(String, Vars), String> {
        if sets_before(&stmts, first) {
            return Err("a nested body assigns a local outside it".to_owned());
        }
        live(&mut stmts, &Vars::new());
        let reads = outer(&stmts, first);
        let (code, _) = self.print_block(&stmts, mode, &mut Vars::new());
        Ok((code, reads))
    }

    /// Decode: `if r.bool()? { Some(..) } else { None }` of an optional statement. The value
    /// takes the place's limit.
    fn option_value(&mut self, s: &Stmt) -> Result<E, String> {
        let elem = s.elem.as_ref().ok_or("optional without element")?;
        let key = s.place.as_ref().and_then(|p| self.limit_key(p));
        self.limit_vars.push((elem.name.clone(), key));
        let value = self.option_inner(s, elem);
        self.limit_vars.pop();
        value
    }

    fn option_inner(&mut self, s: &Stmt, elem: &Param) -> Result<E, String> {
        self.fallible = true;
        let inner = if let Some((value, call)) = self.elem_value(&s.body, elem)? {
            if call {
                value.clone().with_code(format!("{}?", value.code))
            } else {
                value
            }
        } else {
            let (stmts, first) = self.elem_stmts(&s.body, elem)?;
            let (code, reads) = self.print_nested(stmts, first, PrintMode::Block)?;
            let mut e = E::new(format!("{{ {code} }}"));
            e.reads = reads;
            e
        };
        Ok(inner.clone().with_code(format!(
            "if r.bool()? {{ Some({}) }} else {{ None }}",
            inner.code
        )))
    }

    fn slice(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let place = s.place.as_ref().ok_or("slice without place")?;
        let elem = s.elem.as_ref().ok_or("slice without element")?;
        match self.dir {
            Dir::Decode => {
                let call = self.list_call(s)?;
                let e = call.clone().with_code(format!("{}?", call.code)).effect();
                self.store(place, e, out)
            }
            Dir::Encode => {
                let v = self.expr(place)?;
                let list = Self::ref_iter(&v);
                let recv = method_recv(&v.e.code);
                let max = self.limit(Some(place)).map(|max| int_lit(&max.to_string()));
                match s.prefix.as_str() {
                    "varuint32" | "uint8" | "uint32" => {
                        let q = self.q();
                        let len = match &max {
                            Some(max) => format!("length_max({recv}.len(), {max})"),
                            None => format!("length({recv}.len())"),
                        };
                        let code = format!("w.{}({len}{q})", s.prefix);
                        out.push(S::Expr(v.e.clone().with_code(code).effect()));
                    }
                    "" => {
                        let len = self.expr(s.len.as_ref().ok_or("slice without length")?)?;
                        let len = self.usize_value(len)?;
                        self.fallible = true;
                        let over =
                            max.map_or_else(String::new, |max| format!(" || {recv}.len() > {max}"));
                        let cond = len
                            .clone()
                            .with(&v.e)
                            .with_code(format!("{recv}.len() != {}{over}", len.code));
                        out.push(S::If {
                            cond: Cond::Plain { e: cond, neg: None },
                            then: vec![S::Fail(E::new("Error::Size".to_owned()))],
                            els: Vec::new(),
                        });
                    }
                    other => return Err(format!("unsupported slice prefix {other}")),
                }
                let key = self.limit_key(place);
                self.limit_vars
                    .push((elem.name.clone(), key.map(|k| format!("{k}[]"))));
                let body = self.encode_elem_body(&s.body, elem);
                self.limit_vars.pop();
                let (pat, body) = body?;
                let code = format!("for {pat} in {} {{ {} }}", list.code, body.code);
                out.push(S::Expr(list.with(&body).with_code(code).effect()));
                Ok(())
            }
        }
    }

    /// Encode: the pattern and printed body run on each element (or the optional's value).
    fn encode_elem_body(&mut self, body: &[Stmt], elem: &Param) -> Result<(String, E), String> {
        self.scopes.push(BTreeMap::new());
        let name = param_name(&elem.name)?;
        let copy = self.m.is_copy(&elem.ty);
        let kind = if copy { BKind::Owned } else { BKind::Ref };
        self.bind(
            &elem.name,
            Binding {
                code: name.clone(),
                ty: elem.ty.clone(),
                kind,
                var: None,
                external: true,
                borrowed: false,
            },
        );
        let first = self.vars.len();
        let mut out = Vec::new();
        let result = self.stmts(body, &mut out);
        self.scopes.pop();
        result?;
        let (code, reads) = self.print_nested(out, first, PrintMode::Loop)?;
        let mut e = E::new(code);
        e.reads = reads;
        let pat = if copy && mentions_name(&e.code, &name) {
            format!("&{name}")
        } else if mentions_name(&e.code, &name) {
            name
        } else {
            "_".to_owned()
        };
        Ok((pat, e.effect()))
    }

    fn optional(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let place = s.place.as_ref().ok_or("optional without place")?;
        let elem = s.elem.as_ref().ok_or("optional without element")?;
        match self.dir {
            Dir::Decode => {
                let value = self.option_value(s)?;
                self.store(place, value.effect(), out)
            }
            Dir::Encode => {
                let v = self.expr(place)?;
                let recv = method_recv(&v.e.code);
                out.push(S::Expr(
                    v.e.clone()
                        .with_code(format!("w.bool({recv}.is_some())"))
                        .effect(),
                ));
                let key = self.limit_key(place);
                self.limit_vars.push((elem.name.clone(), key));
                let body = self.encode_elem_body(&s.body, elem);
                self.limit_vars.pop();
                let (pat, body) = body?;
                if body.code.trim().is_empty() {
                    return Ok(());
                }
                let copy = self.m.is_copy(&elem.ty);
                let scrut = if v.form == Form::Place && !copy {
                    format!("&{}", v.e.code)
                } else {
                    v.e.code.clone()
                };
                let pat = pat.trim_start_matches('&');
                let code = format!("if let Some({pat}) = {scrut} {{ {} }}", body.code);
                out.push(S::Expr(v.e.clone().with(&body).with_code(code).effect()));
                Ok(())
            }
        }
    }

    fn integer_func(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let place = s.place.as_ref().ok_or("integer_func without place")?;
        let elem = s.elem.as_ref().ok_or("integer_func without element")?;
        let [inner] = s.body.as_slice() else {
            return Err("integer_func body is not one IO call".to_owned());
        };
        if inner.kind != "io" || inner.place.as_ref().is_none_or(|p| p.name != elem.name) {
            return Err("integer_func body is not one IO call on its element".to_owned());
        }
        let wire = model::basic(&elem.ty.name)?;
        let spec = model::io_spec(&inner.method).ok_or("unknown IO method")?;
        let target = self.expr(place)?;
        let prim = target.prim.ok_or("integer_func on a non-integer")?;
        match self.dir {
            Dir::Decode => {
                let call = self.io_read_call(inner)?;
                let code = self.convert_code(&format!("{call}?"), wire, prim)?;
                self.store(place, E::new(code).effect(), out)
            }
            Dir::Encode => {
                let v = self.copy_value(target)?;
                // A value that does not fit a narrower wire type is an error, where
                // gophertunnel truncates it (encoding never truncates).
                let code = if bits(wire) < bits(prim) {
                    let owner = place.x.as_deref().and_then(|x| x.ty.as_ref());
                    let field =
                        field_rust_name(owner.map_or("", |t| t.name.as_str()), &place.name)?;
                    let field = str_lit(field.trim_start_matches("r#"));
                    let q = self.q();
                    format!(
                        "{wire}::try_from({}).map_err(|_| Error::InvalidValue {{ field: {field}, reason: \"out of range for its wire type\" }}){q}",
                        v.code
                    )
                } else {
                    self.convert_code(&v.code, prim, wire)?
                };
                let q = if spec.write_fails { self.q() } else { "" };
                out.push(S::Expr(
                    v.with_code(format!("w.{}({code}){q}", spec.rust)).effect(),
                ));
                Ok(())
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one match arm per call target"
    )]
    fn call_stmt(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        match s.target.as_str() {
            "function" => {
                if let Some(method) = Model::hand_function(&s.func) {
                    let [arg] = s.args.as_slice() else {
                        return Err(format!("{} takes one argument", s.func));
                    };
                    let q = self.q();
                    return match self.dir {
                        Dir::Decode => {
                            self.store(arg, E::new(format!("r.{method}(){q}")).effect(), out)
                        }
                        Dir::Encode => {
                            let val = self.expr(arg)?;
                            let arg_expr = Self::ref_arg(&val);
                            out.push(S::Expr(
                                arg_expr
                                    .clone()
                                    .with_code(format!("w.{method}({}){q}", arg_expr.code))
                                    .effect(),
                            ));
                            Ok(())
                        }
                    };
                }
                let func = *self
                    .m
                    .functions
                    .get(s.func.as_str())
                    .ok_or_else(|| format!("unknown function {}", s.func))?;
                let sum = self
                    .sums
                    .get(&(s.func.clone(), self.dir == Dir::Decode))
                    .cloned()
                    .ok_or_else(|| format!("no summary for {}", s.func))?;
                let all: Vec<&Param> = func.receiver.iter().chain(&func.params).collect();
                let args: Vec<&Expr> = s.recv.iter().chain(&s.args).collect();
                let mut codes = vec![self.r().to_owned()];
                let mut call = E::default();
                for (i, (param, arg_expr)) in all.iter().zip(&args).enumerate() {
                    if !sum.used.get(i).copied().unwrap_or(true) {
                        continue;
                    }
                    let val = self.expr(arg_expr)?;
                    let arg = if param.by_ref {
                        match self.dir {
                            Dir::Decode => Self::mut_arg(&val),
                            Dir::Encode if self.m.is_copy(&param.ty) => self.copy_value(val)?,
                            Dir::Encode => Self::ref_arg(&val),
                        }
                    } else if is_string(&param.ty) {
                        Self::ref_arg(&val)
                    } else {
                        self.copy_value(val)?
                    };
                    call = call.with(&arg);
                    codes.push(arg.code);
                }
                if !func.io {
                    return Err(format!("{}: a pure function as a statement", s.func));
                }
                let prefix = match self.dir {
                    Dir::Decode => "decode_",
                    Dir::Encode => "encode_",
                };
                let q = if sum.fallible { self.q() } else { "" };
                let name = self.fn_path(&s.func, prefix)?;
                let call = call
                    .with_code(format!("{name}({}){q}", codes.join(", ")))
                    .effect();
                if self.dir == Dir::Encode && !sum.in_out.is_empty() {
                    let [i] = sum.in_out.as_slice() else {
                        return Err(format!("{} has more than one in-out parameter", s.func));
                    };
                    let arg_expr = args.get(*i).ok_or("in-out argument missing")?;
                    return self.store(arg_expr, call, out);
                }
                out.push(S::Expr(call));
                Ok(())
            }
            "enum" => {
                let en = *self
                    .m
                    .enums
                    .get(s.func.as_str())
                    .ok_or_else(|| format!("unknown enum {}", s.func))?;
                let out_index = en
                    .params
                    .iter()
                    .position(|param| param.name == en.output)
                    .ok_or_else(|| {
                        format!(
                            "{}: a call statement of an enum that returns its value",
                            s.func
                        )
                    })?;
                let mut codes = Vec::new();
                let mut call = E::default();
                for (i, arg_expr) in s.args.iter().enumerate() {
                    if i == out_index {
                        continue;
                    }
                    let val = self.expr(arg_expr)?;
                    let arg = if is_string(&val.ty) {
                        Self::ref_arg(&val)
                    } else {
                        self.copy_value(val)?
                    };
                    call = call.with(&arg);
                    codes.push(arg.code);
                }
                let name = self.fn_path(&s.func, "")?;
                let q = self.q();
                let call = call
                    .with_code(format!("{name}({}){q}", codes.join(", ")))
                    .effect();
                let target = s
                    .args
                    .get(out_index)
                    .ok_or("enum output argument missing")?;
                self.store(target, call, out)
            }
            other => Err(format!("unsupported call statement target {other}")),
        }
    }

    /// The Rust path of a generated function, with `prefix` (`decode_`, `encode_`).
    fn fn_path(&self, go: &str, prefix: &str) -> Result<String, String> {
        let name = fn_name(go, prefix)?;
        Ok(
            if model::module_of(go) == Module::Types && self.module == Module::Packet {
                format!("super::types::{name}")
            } else {
                name
            },
        )
    }

    /// Assigns a value to a Go place.
    fn store(&mut self, place: &Expr, rhs: E, out: &mut Vec<S>) -> Result<(), String> {
        if place.kind == "blank" {
            if rhs.effects {
                out.push(S::Expr(rhs));
            }
            return Ok(());
        }
        if let Some(path) = place_path(place)
            && let Some(&id) = self.shadows.get(&path)
        {
            out.push(S::Set { var: id, rhs });
            return Ok(());
        }
        let root =
            root_name(place).ok_or_else(|| format!("unsupported assignment target {place:?}"))?;
        let b = self.lookup(&root)?;
        if place.kind == "var"
            && let Some(id) = b.var
        {
            out.push(S::Set { var: id, rhs });
            return Ok(());
        }
        if self.dir == Dir::Encode && b.external {
            // A writer's assignment to what it writes, which nothing reads afterwards.
            if rhs.effects {
                out.push(S::Expr(rhs));
            }
            return Ok(());
        }
        let target = self.expr(place)?;
        let field = match (place.kind.as_str(), place.x.as_deref()) {
            ("field", Some(x)) if x.kind == "var" => {
                let t = x.ty.clone().ok_or("field of untyped base")?;
                Some(self.field_of(&t, &place.name)?.0)
            }
            _ => None,
        };
        let mut reads = target.e.reads.clone();
        reads.extend(b.var);
        let code = if place.kind == "var" {
            b.code.clone()
        } else {
            target.e.code
        };
        out.push(S::Store {
            place: code,
            root: b.var,
            field,
            reads,
            rhs,
        });
        Ok(())
    }

    /// Whether a store to this place is dropped: an encoder's write to what it writes.
    fn drops_store(&self, place: &Expr) -> Result<bool, String> {
        if self.dir != Dir::Encode
            || place_path(place).is_some_and(|p| self.shadows.contains_key(&p))
        {
            return Ok(false);
        }
        let root =
            root_name(place).ok_or_else(|| format!("unsupported assignment target {place:?}"))?;
        let b = self.lookup(&root)?;
        Ok(b.external && !(place.kind == "var" && b.var.is_some()))
    }

    fn assign(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        match (s.lhs.as_slice(), s.rhs.as_slice()) {
            ([lhs_expr], [rhs_expr]) if s.op == ":=" => {
                let val = self.expr(rhs_expr)?;
                let ty = rhs_expr.ty.clone().ok_or("untyped assignment")?;
                // Encoders borrow; decoders own, except arrays of string constants.
                let borrows = match self.dir {
                    Dir::Encode => !(val.form == Form::Owned && is_string(&ty)),
                    Dir::Decode => rhs_expr.kind == "composite" && ty.kind == "array",
                };
                let holding = if borrows {
                    Holding::Borrowed
                } else {
                    Holding::Owned
                };
                let mut init = self.local_value(val, holding)?;
                if borrows && is_string(&ty) {
                    // `let x: &str = *y` derefs automatically (clippy::explicit_auto_deref).
                    if init.code.starts_with('*') {
                        init.code.remove(0);
                    }
                }
                let id = self.declare(&lhs_expr.name, &ty, holding)?;
                out.push(S::Let {
                    vars: vec![Some(id)],
                    init: Some(init),
                    default_of: None,
                });
                Ok(())
            }
            ([lhs_expr], [rhs_expr]) if s.op == "=" => {
                if self.drops_store(lhs_expr)? && !may_fail(rhs_expr) {
                    return Ok(());
                }
                let val = self.expr(rhs_expr)?;
                let local = lhs_expr.kind == "var"
                    && self.lookup(&lhs_expr.name).is_ok_and(|b| b.var.is_some());
                let rhs = if local {
                    let holding = if self
                        .lookup(&lhs_expr.name)
                        .is_ok_and(|b| b.kind == BKind::Ref)
                    {
                        Holding::Borrowed
                    } else {
                        Holding::Owned
                    };
                    self.local_value(val, holding)?
                } else {
                    match self.dir {
                        Dir::Decode => Self::owned_value(val),
                        Dir::Encode => val.e,
                    }
                };
                self.store(lhs_expr, rhs, out)
            }
            ([lhs_expr, rest @ ..], [rhs_expr])
                if rest.iter().all(|x| x.kind == "blank") && rhs_expr.kind == "optional_value" =>
            {
                // `place, _ = opt.Value()`
                let opt = self.expr(rhs_expr.x.as_deref().ok_or("Value without receiver")?)?;
                let value = match self.dir {
                    Dir::Decode => {
                        let owned = Self::owned_value(opt);
                        owned
                            .clone()
                            .with_code(format!("{}.unwrap_or_default()", owned.code))
                    }
                    Dir::Encode => opt
                        .e
                        .clone()
                        .with_code(format!("{}.unwrap_or_default()", opt.e.code)),
                };
                self.store(lhs_expr, value, out)
            }
            (lhs, [rhs_expr]) if s.op == ":=" && lhs.len() > 1 => {
                let val = self.expr(rhs_expr)?;
                let types = match &rhs_expr.ty {
                    Some(ty) if ty.kind == "tuple" => ty.elems.clone(),
                    _ => return Err("tuple assignment from a non-tuple".to_owned()),
                };
                let mut ids = Vec::new();
                for (lhs_expr, ty) in lhs.iter().zip(&types) {
                    if lhs_expr.kind == "blank" {
                        ids.push(None);
                    } else {
                        let holding = self.direction_holding();
                        ids.push(Some(self.declare_unannotated(
                            &lhs_expr.name,
                            ty,
                            holding,
                        )?));
                    }
                }
                out.push(S::Let {
                    vars: ids,
                    init: Some(val.e),
                    default_of: None,
                });
                Ok(())
            }
            _ => Err(format!("unsupported assignment {:?}", s.op)),
        }
    }

    fn if_stmt(&mut self, s: &Stmt) -> Result<S, String> {
        let cond_expr = s.cond.as_ref().ok_or("if without condition")?;
        let cond = match s.init.as_slice() {
            [] => self.cond(cond_expr)?,
            [init] => self.if_let(init, cond_expr)?,
            _ => return Err("if with several init statements".to_owned()),
        };
        let then = self.scoped(&s.then)?;
        let els = self.scoped(&s.els)?;
        Ok(S::If { cond, then, els })
    }

    /// `if v, ok := opt.Value(); ok [&& c]` and `if err := f(); err != nil`.
    fn if_let(&mut self, init: &Stmt, cond: &Expr) -> Result<Cond, String> {
        let ([l, rest @ ..], [r]) = (init.lhs.as_slice(), init.rhs.as_slice()) else {
            return Err("unsupported if init".to_owned());
        };
        if init.kind != "assign" || init.op != ":=" {
            return Err("unsupported if init".to_owned());
        }
        if r.kind == "optional_value" {
            let [ok] = rest else {
                return Err("unsupported Value() init".to_owned());
            };
            let opt_expr = r.x.as_deref().ok_or("Value without receiver")?;
            let opt = self.expr(opt_expr)?;
            let elem_t = opt
                .ty
                .elem
                .as_deref()
                .cloned()
                .ok_or("Value of a non-optional")?;
            let copy = self.m.is_copy(&elem_t);
            let scrut = if opt.form == Form::Place && !copy {
                format!("&{}", opt.e.code)
            } else {
                opt.e.code.clone()
            };
            let borrowed_local =
                opt_expr.kind == "var" && self.lookup(&opt_expr.name).is_ok_and(|b| b.borrowed);
            let by_ref =
                !copy && (opt.form == Form::Place || opt.form == Form::Ref || borrowed_local);
            let id = self.new_var(&l.name, None)?;
            let kind = if by_ref { BKind::Ref } else { BKind::Owned };
            self.bind(
                &l.name,
                Binding {
                    code: self.name(id),
                    ty: elem_t,
                    kind,
                    var: Some(id),
                    external: false,
                    borrowed: false,
                },
            );
            let extra = match (cond.kind.as_str(), cond.op.as_str()) {
                ("var", _) if cond.name == ok.name => None,
                ("binary", "&&")
                    if cond
                        .x
                        .as_deref()
                        .is_some_and(|x| x.kind == "var" && x.name == ok.name) =>
                {
                    Some(
                        self.expr(cond.y.as_deref().ok_or("&& without right side")?)?
                            .e,
                    )
                }
                _ => return Err("unsupported Value() condition".to_owned()),
            };
            return Ok(Cond::Let {
                var: id,
                expr: opt.e.clone().with_code(scrut),
                extra,
            });
        }
        let is_err = r.ty.as_ref().is_some_and(|t| t.kind == "error");
        let checks_nil = cond.kind == "binary"
            && cond.op == "!="
            && cond
                .x
                .as_deref()
                .is_some_and(|x| x.kind == "var" && x.name == l.name)
            && cond.y.as_deref().is_some_and(|y| y.kind == "nil");
        if is_err && rest.is_empty() && checks_nil {
            let v = self.expr(r)?;
            let id = self.new_var(&l.name, None)?;
            let ty = Type {
                kind: "error".to_owned(),
                ..basic_ty("")
            };
            self.bind(
                &l.name,
                Binding {
                    code: self.name(id),
                    ty,
                    kind: BKind::Message,
                    var: Some(id),
                    external: false,
                    borrowed: false,
                },
            );
            return Ok(Cond::Let {
                var: id,
                expr: v.e,
                extra: None,
            });
        }
        Err("unsupported if init".to_owned())
    }

    fn cond(&mut self, e: &Expr) -> Result<Cond, String> {
        let v = self.expr(e)?;
        let neg = self.negate(e)?;
        Ok(Cond::Plain { e: v.e, neg })
    }

    /// The negation of a condition when it is simpler than `!(..)`.
    fn negate(&mut self, e: &Expr) -> Result<Option<String>, String> {
        if e.kind == "unary" && e.op == "!" {
            let x = self.expr(e.x.as_deref().ok_or("! without operand")?)?;
            return Ok(Some(x.e.code));
        }
        if e.kind == "binary" {
            let flipped = match e.op.as_str() {
                "==" => "!=",
                "!=" => "==",
                "<" => ">=",
                ">=" => "<",
                ">" => "<=",
                "<=" => ">",
                _ => return Ok(None),
            };
            let x = e.x.as_deref().and_then(|x| x.ty.as_ref()).and_then(prim_of);
            if x.is_some_and(|p| p.starts_with('f')) {
                return Ok(None);
            }
            let mut flipped_expr = e.clone();
            flipped.clone_into(&mut flipped_expr.op);
            return Ok(Some(self.expr(&flipped_expr)?.e.code));
        }
        Ok(None)
    }

    fn switch(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        if !s.init.is_empty() {
            return Err("switch with init".to_owned());
        }
        let Some(tag) = &s.tag else {
            return Err("switch without a tag".to_owned());
        };
        let v = self.expr(tag)?;
        let scrut = if is_string(&v.ty) {
            match v.form {
                Form::Ref => v.e.clone(),
                _ => v.e.clone().with_code(format!("{}.as_str()", v.e.code)),
            }
        } else {
            self.copy_value(v)?
        };
        let mut arms = Vec::new();
        for c in &s.cases {
            let pats = if c.default {
                Vec::new()
            } else {
                c.values
                    .iter()
                    .map(|x| self.pattern(x))
                    .collect::<Result<Vec<_>, String>>()?
            };
            let body = self.scoped(&c.body)?;
            arms.push((pats, body));
        }
        out.push(S::Match { scrut, arms });
        Ok(())
    }

    /// A case value as a pattern.
    fn pattern(&mut self, e: &Expr) -> Result<String, String> {
        match e.kind.as_str() {
            "lit" | "const" => Ok(self.expr(e)?.e.code),
            _ => Err(format!("unsupported case value {e:?}")),
        }
    }

    fn for_stmt(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        // for i := 0; i < N; i++ { ... X[i] ... } over an array X of length N.
        let ([init], [post]) = (s.init.as_slice(), s.post.as_slice()) else {
            return Err("unsupported for loop".to_owned());
        };
        let var = init.lhs.first().map(|l| l.name.clone()).unwrap_or_default();
        let from_zero = init.op == ":="
            && init
                .rhs
                .first()
                .is_some_and(|r| r.value.as_deref() == Some("0"));
        let by_one = post.op == "+="
            && post
                .rhs
                .first()
                .is_some_and(|r| r.value.as_deref() == Some("1"));
        let cond = s.cond.as_ref().ok_or("for without condition")?;
        let bound = cond.y.as_deref().and_then(|y| y.value.clone());
        if !from_zero || !by_one || cond.op != "<" || bound.is_none() {
            return Err("unsupported for loop".to_owned());
        }
        let base = index_base(&s.body, &var).ok_or("for loop index used other than as an index")?;
        let len = base.ty.as_ref().and_then(|t| t.len).map(|l| l.to_string());
        if len != bound {
            return Err("for loop bound differs from the array length".to_owned());
        }
        self.element_loop(&var, &base, &s.body, out)
    }

    fn range(&mut self, s: &Stmt, out: &mut Vec<S>) -> Result<(), String> {
        let x = s.x.as_ref().ok_or("range without operand")?;
        let key = s
            .key
            .as_ref()
            .filter(|k| k.kind != "blank")
            .map(|k| k.name.clone());
        let value = s
            .range_value
            .as_ref()
            .filter(|k| k.kind != "blank")
            .map(|k| k.name.clone());
        if let (Some(k), None) = (&key, &value)
            && index_base(&s.body, k).is_some_and(|b| same_expr(&b, x))
        {
            return self.element_loop(k, x, &s.body, out);
        }
        let v = self.expr(x)?;
        let elem_t =
            v.ty.elem
                .as_deref()
                .cloned()
                .ok_or("range over a non-list")?;
        let list = Self::ref_iter(&v);
        self.scopes.push(BTreeMap::new());
        let first = self.vars.len();
        let copy = self.m.is_copy(&elem_t);
        let key_name = match &key {
            Some(k) => {
                let name = param_name(k)?;
                self.usize_names.insert(k.clone());
                self.bind(
                    k,
                    Binding {
                        code: name.clone(),
                        ty: basic_ty("int"),
                        kind: BKind::Owned,
                        var: None,
                        external: true,
                        borrowed: false,
                    },
                );
                Some(name)
            }
            None => None,
        };
        let value_name = match &value {
            Some(vn) => {
                let name = param_name(vn)?;
                let kind = if copy { BKind::Owned } else { BKind::Ref };
                self.bind(
                    vn,
                    Binding {
                        code: name.clone(),
                        ty: elem_t.clone(),
                        kind,
                        var: None,
                        external: true,
                        borrowed: false,
                    },
                );
                Some(name)
            }
            None => None,
        };
        let mut body = Vec::new();
        let result = self.stmts(&s.body, &mut body);
        self.scopes.pop();
        result?;
        let (printed, reads) = self.print_nested(body, first, PrintMode::Loop)?;
        let used = |n: &Option<String>| n.as_ref().filter(|n| mentions_name(&printed, n)).cloned();
        let vpat = match used(&value_name) {
            Some(n) if copy => format!("&{n}"),
            Some(n) => n,
            None => "_".to_owned(),
        };
        let (head, iter) = match used(&key_name) {
            Some(k) => {
                let base = match list.code.strip_prefix('&') {
                    Some(inner) => inner.to_owned(),
                    None => method_recv(&list.code),
                };
                (
                    format!("({k}, {vpat})"),
                    format!("{base}.iter().enumerate()"),
                )
            }
            None => (vpat, list.code.clone()),
        };
        let mut e = list.with_code(format!("for {head} in {iter} {{ {printed} }}"));
        e.reads.extend(reads);
        out.push(S::Expr(e.effect()));
        Ok(())
    }

    /// A loop over an array or slice whose body uses the index only to index it: the element
    /// takes the place of `base[var]`.
    fn element_loop(
        &mut self,
        var: &str,
        base: &Expr,
        body: &[Stmt],
        out: &mut Vec<S>,
    ) -> Result<(), String> {
        let bv = self.expr(base)?;
        let elem_t = bv
            .ty
            .elem
            .as_deref()
            .cloned()
            .or_else(|| {
                self.m
                    .types
                    .get(bv.ty.name.as_str())
                    .and_then(|d| d.underlying.as_ref())
                    .and_then(|u| u.elem.as_deref().cloned())
            })
            .ok_or("element loop over a non-list")?;
        let writes = self.dir == Dir::Decode && writes_index(body, var);
        let name = param_name(var)?;
        let deref = bv.e.code.strip_prefix('*').map(str::to_owned);
        let iter = match (writes, deref) {
            (true | false, Some(inner)) => inner,
            (true, None) => format!("&mut {}", bv.e.code),
            (false, None) if bv.form == Form::Ref => bv.e.code.clone(),
            (false, None) => format!("&{}", bv.e.code),
        };
        let copy = self.m.is_copy(&elem_t);
        let (pat, kind, code) = if writes {
            (name.clone(), BKind::RefMut, format!("*{name}"))
        } else if copy {
            (format!("&{name}"), BKind::Owned, name.clone())
        } else {
            (name.clone(), BKind::Ref, name.clone())
        };
        self.scopes.push(BTreeMap::new());
        self.bind(
            &format!("#elem:{var}"),
            Binding {
                code,
                ty: elem_t,
                kind,
                var: None,
                external: true,
                borrowed: false,
            },
        );
        let first = self.vars.len();
        let renamed = replace_index(body, var, base);
        let mut stmts = Vec::new();
        let result = self.stmts(&renamed, &mut stmts);
        self.scopes.pop();
        result?;
        let (printed, reads) = self.print_nested(stmts, first, PrintMode::Loop)?;
        let mut e =
            bv.e.clone()
                .with_code(format!("for {pat} in {iter} {{ {printed} }}"));
        if writes {
            e.muts.extend(&bv.e.reads);
        }
        e.reads.extend(reads);
        out.push(S::Expr(e.effect()));
        Ok(())
    }

    fn field_of(&self, t: &Type, go_field: &str) -> Result<(String, Type), String> {
        let def = self
            .m
            .struct_def(&t.name)
            .ok_or_else(|| format!("field {go_field} of non-struct {}", t.name))?;
        let f = def
            .fields
            .iter()
            .find(|f| f.name == go_field)
            .ok_or_else(|| format!("{} has no field {go_field}", t.name))?;
        Ok((field_rust_name(&t.name, &f.name)?, f.ty.clone()))
    }

    fn val(&self, e: E, ty: Type, form: Form) -> Val {
        let prim = prim_of(&ty);
        let form = if self.m.is_copy(&ty) && form != Form::Ref {
            Form::Copy
        } else {
            form
        };
        Val { e, ty, form, prim }
    }

    fn shadow(&self, e: &Expr) -> Option<E> {
        let id = *self.shadows.get(&place_path(e)?)?;
        let mut x = E::new(self.name(id));
        x.reads.insert(id);
        Some(x)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one match arm per Go expression kind"
    )]
    fn expr(&mut self, e: &Expr) -> Result<Val, String> {
        let ty = e.ty.clone().unwrap_or_else(|| basic_ty("untyped int"));
        match e.kind.as_str() {
            "var" | "field" if self.shadow(e).is_some() => {
                let x = self.shadow(e).unwrap_or_default();
                Ok(self.val(x, ty, Form::Copy))
            }
            "var" => {
                let b = self.lookup(&e.name)?;
                let mut x = E::new(b.code.clone());
                x.reads.extend(b.var);
                let copy = self.m.is_copy(&b.ty);
                let mut v = match b.kind {
                    BKind::Owned => {
                        let form = if b.var.is_some() {
                            Form::Local
                        } else {
                            Form::Place
                        };
                        self.val(x, b.ty.clone(), form)
                    }
                    BKind::Ref if copy => self.val(
                        x.clone().with_code(format!("*{}", x.code)),
                        b.ty.clone(),
                        Form::Copy,
                    ),
                    BKind::Ref => Val {
                        e: x,
                        ty: b.ty.clone(),
                        form: Form::Ref,
                        prim: None,
                    },
                    BKind::RefMut if copy => self.val(x, b.ty.clone(), Form::Copy),
                    BKind::RefMut => Val {
                        e: x,
                        ty: b.ty.clone(),
                        form: Form::Place,
                        prim: None,
                    },
                    BKind::Message => Val {
                        e: x,
                        ty: b.ty.clone(),
                        form: Form::Copy,
                        prim: None,
                    },
                };
                if self.usize_names.contains(&e.name) {
                    v.prim = Some("usize");
                }
                Ok(v)
            }
            "field" => {
                let base_expr = e.x.as_deref().ok_or("field without base")?;
                let base = self.expr(base_expr)?;
                let base_t = base_expr.ty.clone().ok_or("field of untyped base")?;
                let (name, _) = self.field_of(&base_t, &e.name)?;
                let code = format!("{}.{name}", base.e.code.trim_start_matches('*'));
                Ok(self.val(base.e.with_code(code), ty, Form::Place))
            }
            "index" => {
                let base_expr = e.x.as_deref().ok_or("index without base")?;
                let base = self.expr(base_expr)?;
                let index = e.index.as_deref().ok_or("index without index")?;
                let fixed = base.ty.kind == "array"
                    || (base.ty.kind == "external" && base.ty.name.starts_with("mgl32.Vec"))
                    || self
                        .m
                        .types
                        .get(base.ty.name.as_str())
                        .is_some_and(|d| d.kind == "array");
                if fixed && index.kind == "lit" {
                    let i = index.value.clone().unwrap_or_default();
                    let code = format!("{}[{i}]", base.e.code.trim_start_matches('*'));
                    return Ok(self.val(base.e.with_code(code), ty, Form::Place));
                }
                let i = self.expr(index)?;
                let i = self.copy_value(i)?;
                self.helpers.insert("go_index".to_owned());
                let q = self.q();
                let helper = self.helper_path("go_index");
                let list = Self::ref_iter(&base);
                let code = format!("{helper}({}, {}){q}", list.code, i.code);
                let x = list.with(&i).with_code(code).effect();
                if self.m.is_copy(&ty) || is_string(&ty) {
                    let form = if is_string(&ty) {
                        Form::Ref
                    } else {
                        Form::Copy
                    };
                    let x = x.clone().with_code(format!("*{}", x.code));
                    Ok(Val {
                        e: x,
                        prim: prim_of(&ty),
                        ty,
                        form,
                    })
                } else {
                    Ok(Val {
                        e: x,
                        ty,
                        form: Form::Ref,
                        prim: None,
                    })
                }
            }
            "slice_of" => {
                if e.low.is_some() {
                    return Err("slice expression with a low bound".to_owned());
                }
                let base = self.expr(e.x.as_deref().ok_or("slice without base")?)?;
                let high = self.expr(e.high.as_deref().ok_or("slice without high bound")?)?;
                let high = self.usize_value(high)?;
                self.helpers.insert("go_prefix".to_owned());
                let q = self.q();
                let helper = self.helper_path("go_prefix");
                let list = Self::ref_iter(&base);
                let code = format!("{helper}({}, {}){q}", list.code, high.code);
                Ok(Val {
                    e: list.with(&high).with_code(code).effect(),
                    ty,
                    form: Form::Ref,
                    prim: None,
                })
            }
            "const" | "lit" => Self::constant(e, ty),
            // An untyped nil only appears in comparisons (handled there) and returns of errors.
            "nil" => Ok(Val {
                e: E::new("None".to_owned()),
                ty,
                form: Form::Copy,
                prim: None,
            }),
            "unary" => {
                let x = self.expr(e.x.as_deref().ok_or("unary without operand")?)?;
                let prim = x.prim;
                let xc = self.copy_value(x)?;
                let code = match e.op.as_str() {
                    "!" | "^" => format!("!{}", paren(&xc.code)),
                    "-" if is_int(prim) => format!("{}.wrapping_neg()", paren(&xc.code)),
                    "-" => format!("-{}", paren(&xc.code)),
                    other => return Err(format!("unsupported unary {other}")),
                };
                let mut v = self.val(xc.with_code(code), ty, Form::Copy);
                v.prim = prim.or(v.prim);
                Ok(v)
            }
            "binary" => self.binary(e, ty),
            "convert" => {
                let xe = e.x.as_deref().ok_or("convert without operand")?;
                let x = self.expr(xe)?;
                let to = prim_of(&ty);
                match (x.prim, to) {
                    (Some(from), Some(to)) => {
                        let xc = self.copy_value(x)?;
                        let code = self.convert_code(&xc.code, from, to)?;
                        Ok(Val {
                            e: xc.with_code(code),
                            ty,
                            form: Form::Copy,
                            prim: Some(to),
                        })
                    }
                    (None, Some(to)) if matches!(xe.kind.as_str(), "lit" | "const") => Ok(Val {
                        prim: Some(to),
                        ty,
                        ..x
                    }),
                    _ if x.ty == ty => Ok(x),
                    _ => Err(format!("unsupported conversion to {ty:?}")),
                }
            }
            "call" => self.call_expr(e, ty),
            "composite" => self.composite(e, ty),
            "option" => {
                let x = self.expr(e.x.as_deref().ok_or("Option without value")?)?;
                let inner = match self.dir {
                    Dir::Decode => Self::owned_value(x),
                    Dir::Encode => self.ref_value_or_copy(x)?,
                };
                let code = format!("Some({})", inner.code);
                Ok(Val {
                    e: inner.with_code(code),
                    ty,
                    form: Form::Owned,
                    prim: None,
                })
            }
            "shield_id" => {
                let code = format!("{}.shield_id()", self.r());
                Ok(self.val(E::new(code), basic_ty("int32"), Form::Copy))
            }
            other => Err(format!("unsupported expression {other}")),
        }
    }

    fn constant(e: &Expr, ty: Type) -> Result<Val, String> {
        let value = e.value.clone().ok_or("constant without value")?;
        if is_string(&ty) {
            return Ok(Val {
                e: E::new(str_lit(&value)),
                ty: basic_ty("string"),
                form: Form::Ref,
                prim: None,
            });
        }
        let prim = prim_of(&ty);
        let code = match prim {
            Some("bool") => value,
            Some("f32" | "f64") => float_lit(&value)?,
            Some(_) => int_lit(&value),
            None => return Err(format!("unsupported constant type {ty:?}")),
        };
        let untyped = ty.name.starts_with("untyped");
        Ok(Val {
            e: E::new(code),
            ty,
            form: Form::Copy,
            prim: if untyped { None } else { prim },
        })
    }

    fn binary(&mut self, e: &Expr, ty: Type) -> Result<Val, String> {
        let xe = e.x.as_deref().ok_or("binary without left side")?;
        let ye = e.y.as_deref().ok_or("binary without right side")?;
        let op = e.op.as_str();
        if let Some(val) = self.len_zero(xe, ye, op)? {
            return Ok(val);
        }
        if ye.kind == "nil" || xe.kind == "nil" {
            let other = if ye.kind == "nil" { xe } else { ye };
            let val = self.expr(other)?;
            let code = match op {
                "==" => format!("{}.is_none()", val.e.code),
                "!=" => format!("{}.is_some()", val.e.code),
                _ => return Err("unsupported nil comparison".to_owned()),
            };
            return Ok(self.val(val.e.with_code(code), basic_ty("bool"), Form::Copy));
        }
        let lhs = self.expr(xe)?;
        let rhs = self.expr(ye)?;
        if is_string(&lhs.ty) && op == "+" {
            let a = self.ref_value_or_copy(lhs)?;
            let b = self.ref_value_or_copy(rhs)?;
            let code = format!("[{}, {}].concat()", a.code, b.code);
            return Ok(Val {
                e: a.with(&b).with_code(code),
                ty,
                form: Form::Owned,
                prim: None,
            });
        }
        if is_string(&lhs.ty) {
            // Comparing with "" is `is_empty` (clippy::comparison_to_empty).
            if ye.value.as_deref() == Some("") && matches!(op, "==" | "!=") {
                let not = if op == "!=" { "!" } else { "" };
                let code = format!("{not}{}.is_empty()", method_recv(&lhs.e.code));
                return Ok(self.val(lhs.e.with_code(code), basic_ty("bool"), Form::Copy));
            }
            let code = format!("{} {op} {}", lhs.e.code, rhs.e.code);
            return Ok(self.val(
                lhs.e.with(&rhs.e).with_code(code),
                basic_ty("bool"),
                Form::Copy,
            ));
        }
        // A constant takes the type of the other side.
        let literal = |e: &Expr| {
            matches!(e.kind.as_str(), "lit" | "const")
                && !e.value.as_deref().unwrap_or_default().starts_with('-')
        };
        let (xp, yp) = match (literal(xe), literal(ye)) {
            (false, true) => (lhs.prim, lhs.prim),
            (true, false) => (rhs.prim, rhs.prim),
            _ => (lhs.prim, rhs.prim),
        };
        let mut xc = self.copy_value(lhs)?;
        let mut yc = self.copy_value(rhs)?;
        // Go `int` values from lengths are `usize`; other Go ints are `i64`.
        let prim = match (xp, yp) {
            (Some("usize"), Some("i64")) => {
                xc.code = self.convert_code(&xc.code, "usize", "i64")?;
                Some("i64")
            }
            (Some("i64"), Some("usize")) => {
                yc.code = self.convert_code(&yc.code, "usize", "i64")?;
                Some("i64")
            }
            (a, b) => a.or(b),
        };
        let code = match op {
            "+" | "-" | "*" if is_int(prim) => {
                let name = match op {
                    "+" => "add",
                    "-" => "sub",
                    _ => "mul",
                };
                format!("{}.wrapping_{name}({})", paren(&xc.code), yc.code)
            }
            "&^" => format!("{} & !{}", paren(&xc.code), paren(&yc.code)),
            "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||" | "&" | "|" | "^" | "+" | "-"
            | "*" | "/" => {
                format!("{} {op} {}", paren(&xc.code), paren(&yc.code))
            }
            other => return Err(format!("unsupported operator {other}")),
        };
        let compares = matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||");
        let mut val = self.val(xc.with(&yc).with_code(code), ty, Form::Copy);
        if !compares {
            val.prim = prim;
        }
        Ok(val)
    }

    /// `len(x) == 0` and the like as `is_empty` (`clippy::len_zero`).
    fn len_zero(&mut self, xe: &Expr, ye: &Expr, op: &str) -> Result<Option<Val>, String> {
        if xe.kind != "call" || xe.func != "len" || ye.kind != "lit" {
            return Ok(None);
        }
        let empty = match (op, ye.value.as_deref()) {
            ("==", Some("0")) | ("<", Some("1")) => true,
            ("!=" | ">", Some("0")) | (">=", Some("1")) => false,
            _ => return Ok(None),
        };
        let arg = self.expr(xe.args.first().ok_or("len without argument")?)?;
        let recv = method_recv(&arg.e.code);
        let code = if empty {
            format!("{recv}.is_empty()")
        } else {
            format!("!{recv}.is_empty()")
        };
        Ok(Some(self.val(
            arg.e.with_code(code),
            basic_ty("bool"),
            Form::Copy,
        )))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one match arm per Go call kind"
    )]
    fn call_expr(&mut self, e: &Expr, ty: Type) -> Result<Val, String> {
        match (e.target.as_str(), e.func.as_str()) {
            ("builtin", "len") => {
                let a = self.expr(e.args.first().ok_or("len without argument")?)?;
                let code = format!("{}.len()", method_recv(&a.e.code));
                Ok(Val {
                    e: a.e.with_code(code),
                    ty,
                    form: Form::Copy,
                    prim: Some("usize"),
                })
            }
            ("builtin", "slices.Contains") => {
                let [s, v] = e.args.as_slice() else {
                    return Err("slices.Contains arguments".to_owned());
                };
                let s = self.expr(s)?;
                let v = self.expr(v)?;
                let v = self.copy_value(v)?;
                let code = format!("{}.contains(&{})", method_recv(&s.e.code), v.code);
                Ok(self.val(s.e.with(&v).with_code(code), basic_ty("bool"), Form::Copy))
            }
            ("builtin", "strings.EqualFold") => {
                let [a, b] = e.args.as_slice() else {
                    return Err("strings.EqualFold arguments".to_owned());
                };
                let a = self.expr(a)?;
                let b = self.expr(b)?;
                let br = self.ref_value_or_copy(b)?;
                let code = format!(
                    "{}.eq_ignore_ascii_case({})",
                    method_recv(&a.e.code),
                    br.code
                );
                Ok(self.val(a.e.with(&br).with_code(code), basic_ty("bool"), Form::Copy))
            }
            ("builtin", "strings.TrimPrefix") => {
                let [a, b] = e.args.as_slice() else {
                    return Err("strings.TrimPrefix arguments".to_owned());
                };
                let a = self.expr(a)?;
                let b = self.expr(b)?;
                let code = format!("{0}.strip_prefix({1}).unwrap_or({0})", a.e.code, b.e.code);
                Ok(Val {
                    e: a.e.with(&b.e).with_code(code),
                    ty,
                    form: Form::Ref,
                    prim: None,
                })
            }
            ("builtin", "fmt.Errorf") => {
                let format = e
                    .args
                    .first()
                    .and_then(|a| a.value.clone())
                    .ok_or("Errorf without format")?;
                let code = format!("Some({})", str_lit(&format.replace("%v", "?")));
                Ok(Val {
                    e: E::new(code),
                    ty,
                    form: Form::Copy,
                    prim: None,
                })
            }
            ("builtin", "error.Error") => {
                let recv = self.expr(e.recv.as_deref().ok_or("Error without receiver")?)?;
                Ok(Val {
                    e: recv.e,
                    ty,
                    form: Form::Ref,
                    prim: None,
                })
            }
            ("enum", func) => {
                let en = *self
                    .m
                    .enums
                    .get(func)
                    .ok_or_else(|| format!("unknown enum {func}"))?;
                if en.output != "return" {
                    return Err(format!("{func}: call expression of an assigning enum"));
                }
                let mut codes = Vec::new();
                let mut x = E::default();
                for a in &e.args {
                    let v = self.expr(a)?;
                    let v = if is_string(&v.ty) {
                        Self::ref_arg(&v)
                    } else {
                        self.copy_value(v)?
                    };
                    x = x.with(&v);
                    codes.push(v.code);
                }
                let fails = en.default.iter().any(|s| s.kind == "error");
                let q = if fails { self.q() } else { "" };
                let name = self.fn_path(func, "")?;
                let mut x = x.with_code(format!("{name}({}){q}", codes.join(", ")));
                x.effects |= fails;
                Ok(self.call_val(x, ty, Form::Ref))
            }
            ("function", func) => {
                let f = *self
                    .m
                    .functions
                    .get(func)
                    .ok_or_else(|| format!("unknown function {func}"))?;
                if f.io {
                    return Err(format!("{func}: IO function in an expression"));
                }
                let sum = self
                    .sums
                    .get(&(func.to_owned(), true))
                    .cloned()
                    .ok_or_else(|| format!("no summary for {func}"))?;
                let all: Vec<&Param> = f.receiver.iter().chain(&f.params).collect();
                let args: Vec<&Expr> = e.recv.as_deref().into_iter().chain(&e.args).collect();
                let mut codes = Vec::new();
                let mut x = E::default();
                for (i, (p, a)) in all.iter().zip(&args).enumerate() {
                    if !sum.used.get(i).copied().unwrap_or(true) {
                        continue;
                    }
                    let v = self.expr(a)?;
                    let v = if self.m.is_copy(&p.ty) {
                        self.copy_value(v)?
                    } else {
                        Self::ref_arg(&v)
                    };
                    x = x.with(&v);
                    codes.push(v.code);
                }
                let q = if sum.fallible { self.q() } else { "" };
                let name = self.fn_path(func, "")?;
                let mut x = x.with_code(format!("{name}({}){q}", codes.join(", ")));
                x.effects |= sum.fallible;
                let string_form = if sum.result == "String" {
                    Form::Owned
                } else {
                    Form::Ref
                };
                Ok(self.call_val(x, ty, string_form))
            }
            (target, func) => Err(format!("unsupported call {target} {func}")),
        }
    }

    /// The value of a call result.
    fn call_val(&self, x: E, ty: Type, string_form: Form) -> Val {
        if is_string(&ty) {
            Val {
                e: x,
                ty,
                form: string_form,
                prim: None,
            }
        } else if ty.kind == "error" || ty.kind == "tuple" {
            Val {
                e: x,
                ty,
                form: Form::Owned,
                prim: None,
            }
        } else {
            self.val(x, ty, Form::Owned)
        }
    }

    fn composite(&mut self, e: &Expr, ty: Type) -> Result<Val, String> {
        match ty.kind.as_str() {
            "optional" if e.fields.is_empty() && e.elems.is_empty() => Ok(Val {
                e: E::new("None".to_owned()),
                ty,
                form: Form::Owned,
                prim: None,
            }),
            "array" => {
                let mut parts = Vec::new();
                let mut x = E::default();
                for el in &e.elems {
                    let v = self.expr(el)?;
                    let v = self.ref_value_or_copy(v)?;
                    x = x.with(&v);
                    parts.push(v.code);
                }
                let code = format!("[{}]", parts.join(", "));
                Ok(Val {
                    e: x.with_code(code),
                    ty,
                    form: Form::Ref,
                    prim: None,
                })
            }
            "named" => {
                let def = self
                    .m
                    .struct_def(&ty.name)
                    .ok_or("composite of a non-struct")?;
                let name = type_path(&self.rust_type(&ty)?);
                let mut parts = Vec::new();
                let mut x = E::default();
                for fv in &e.fields {
                    let (fname, _) = self.field_of(&ty, &fv.name)?;
                    let v = self.expr(&fv.value)?;
                    let v = match self.dir {
                        Dir::Decode => Self::owned_value(v),
                        Dir::Encode if matches!(fv.value.kind.as_str(), "lit" | "const") => v.e,
                        Dir::Encode => self.copy_value(v)?,
                    };
                    x = x.with(&v);
                    parts.push(if v.code == fname {
                        fname
                    } else {
                        format!("{fname}: {}", v.code)
                    });
                }
                if e.fields.len() < def.fields.len() {
                    parts.push(format!("..{name}::default()"));
                }
                let code = format!("{name} {{ {} }}", parts.join(", "));
                Ok(Val {
                    e: x.with_code(code),
                    ty,
                    form: Form::Owned,
                    prim: None,
                })
            }
            _ => Err(format!("unsupported composite literal {ty:?}")),
        }
    }

    /// A `Copy` value.
    fn copy_value(&mut self, v: Val) -> Result<E, String> {
        match v.form {
            Form::Copy => Ok(v.e),
            _ if self.m.is_copy(&v.ty) => Ok(v.e),
            _ => Err(format!("a non-Copy value used by value: {}", v.e.code)),
        }
    }

    /// A reference to pass as a function argument (coerced at the call).
    fn ref_arg(v: &Val) -> E {
        match v.form {
            Form::Ref => v.e.clone(),
            _ => v.e.clone().with_code(format!("&{}", v.e.code)),
        }
    }

    /// A borrowed value with its final type (`&str` for strings), or a `Copy` value.
    fn ref_value_or_copy(&mut self, v: Val) -> Result<E, String> {
        if self.m.is_copy(&v.ty) && v.form != Form::Ref {
            return self.copy_value(v);
        }
        Ok(match v.form {
            Form::Ref | Form::Copy => v.e,
            _ if is_string(&v.ty) => v.e.clone().with_code(format!("{}.as_str()", v.e.code)),
            _ => v.e.clone().with_code(format!("&{}", v.e.code)),
        })
    }

    /// Something to iterate by reference.
    fn ref_iter(v: &Val) -> E {
        match v.form {
            Form::Ref => v.e.clone(),
            _ => match v.e.code.strip_prefix('*') {
                Some(inner) => v.e.clone().with_code(inner.to_owned()),
                None => v.e.clone().with_code(format!("&{}", v.e.code)),
            },
        }
    }

    /// A mutable reference argument (decode).
    fn mut_arg(v: &Val) -> E {
        let mut e = v.e.clone();
        e.muts.extend(&v.e.reads);
        e.code = match e.code.strip_prefix('*') {
            Some(inner) => inner.to_owned(),
            None => format!("&mut {}", e.code),
        };
        e
    }

    /// An owned value (decode): locals move, places clone.
    fn owned_value(v: Val) -> E {
        match v.form {
            Form::Copy | Form::Owned | Form::Local => v.e,
            Form::Ref if is_string(&v.ty) => {
                v.e.clone()
                    .with_code(format!("{}.to_owned()", method_recv(&v.e.code)))
            }
            Form::Place | Form::Ref => {
                v.e.clone()
                    .with_code(format!("{}.clone()", method_recv(&v.e.code)))
            }
        }
    }

    /// The initial or assigned value of a local.
    fn local_value(&mut self, v: Val, holding: Holding) -> Result<E, String> {
        if holding == Holding::Owned {
            return Ok(Self::owned_value(v));
        }
        if v.form == Form::Owned && !self.m.is_copy(&v.ty) {
            return Ok(v.e);
        }
        self.ref_value_or_copy(v)
    }

    fn return_value(&mut self, v: Val) -> Result<E, String> {
        if self.m.is_copy(&v.ty) && v.form != Form::Ref {
            return self.copy_value(v);
        }
        match v.form {
            Form::Owned | Form::Ref | Form::Copy => Ok(v.e),
            Form::Local | Form::Place => self.ref_value_or_copy(v),
        }
    }

    fn i64_value(&mut self, v: Val) -> Result<E, String> {
        let from = v.prim.ok_or("enum value of unknown type")?;
        let e = self.copy_value(v)?;
        let code = self.convert_code(&e.code, from, "i64")?;
        Ok(e.with_code(code))
    }

    fn u64_value(&mut self, v: Val) -> Result<E, String> {
        let from = v.prim.unwrap_or("u32");
        let e = self.copy_value(v)?;
        let code = match from {
            "u64" => e.code.clone(),
            "u8" | "u16" | "u32" => format!("u64::from({})", e.code),
            _ => {
                let q = self.q();
                format!("u64::try_from({}).map_err(|_| Error::Size){q}", e.code)
            }
        };
        Ok(e.with_code(code))
    }

    fn usize_value(&mut self, v: Val) -> Result<E, String> {
        let from = v.prim.unwrap_or("usize");
        let e = self.copy_value(v)?;
        let code = match from {
            "usize" => e.code.clone(),
            "u8" | "u16" => format!("usize::from({})", e.code),
            _ => {
                let q = self.q();
                format!("usize::try_from({}).map_err(|_| Error::Size){q}", e.code)
            }
        };
        Ok(e.with_code(code))
    }

    fn helper_path(&self, name: &str) -> String {
        if self.module == Module::Packet {
            format!("super::types::{name}")
        } else {
            name.to_owned()
        }
    }

    /// A Go integer conversion of `code` from the Rust integer `from` to `to`.
    fn convert_code(&mut self, code: &str, from: &str, to: &str) -> Result<String, String> {
        if from == to {
            return Ok(code.to_owned());
        }
        if !is_int(Some(from)) || !is_int(Some(to)) {
            return Err(format!("unsupported conversion {from} to {to}"));
        }
        if from == "usize" || to == "usize" || bits(to) < bits(from) {
            let name = format!("go_{from}_to_{to}");
            self.helpers.insert(name.clone());
            return Ok(format!("{}({code})", self.helper_path(&name)));
        }
        let to_signed = to.starts_with('i');
        Ok(if bits(to) == bits(from) {
            let method = if to_signed {
                "cast_signed"
            } else {
                "cast_unsigned"
            };
            format!("{}.{method}()", paren(code))
        } else if from.starts_with('i') && !to_signed {
            format!("i{}::from({code}).cast_unsigned()", bits(to))
        } else {
            format!("{to}::from({code})")
        })
    }

    /// The zero value of a Go type as a local initializer.
    fn zero(&mut self, t: &Type) -> Result<E, String> {
        Ok(E::new(match t.kind.as_str() {
            "basic" => match t.name.as_str() {
                "bool" => "false".to_owned(),
                "string" => match self.dir {
                    Dir::Decode => "String::new()".to_owned(),
                    Dir::Encode => "\"\"".to_owned(),
                },
                "float32" | "float64" => "0.0".to_owned(),
                _ => "0".to_owned(),
            },
            "optional" => "None".to_owned(),
            _ if self.dir == Dir::Encode && !self.m.is_copy(t) => {
                return Err(format!("zero value of {t:?} in an encoder"));
            }
            _ => format!("{}::default()", type_path(&self.rust_type(t)?)),
        }))
    }

    /// Prints a cleaned statement list; `init` holds the locals initialized before it (by-value
    /// parameters). Returns the code and the locals that need `mut`.
    fn print_block(&self, stmts: &[S], mode: PrintMode, init: &mut Vars) -> (String, Vars) {
        let folded = fold(stmts);
        let stmts = folded.as_slice();
        let mut muts = Vars::new();
        let mut state = Some(init.clone());
        assigned(stmts, &mut state, &mut muts);
        collect_muts(stmts, &mut muts);
        let mut out = String::new();
        self.print(stmts, &muts, &mut out, mode, Ending::EndsBody);
        (out, muts)
    }

    /// Prints statements; `ending` tells whether they end the body.
    #[expect(
        clippy::too_many_lines,
        reason = "consistency: one match arm per statement kind"
    )]
    fn print(&self, stmts: &[S], muts: &Vars, out: &mut String, mode: PrintMode, ending: Ending) {
        // Late-initialized locals merged into their first assignment
        // (clippy::needless_late_init): statement index to the `let` replacing it.
        let mut merged: BTreeMap<usize, String> = BTreeMap::new();
        let mut i = 0;
        while let Some(s) = stmts.get(i) {
            let is_last = ending == Ending::EndsBody && i + 1 == stmts.len();
            let statement_ending = if is_last {
                Ending::EndsBody
            } else {
                Ending::Inside
            };
            if let Some(text) = merged.remove(&i) {
                out.push_str(&text);
                i += 1;
                continue;
            }
            match s {
                S::Let {
                    vars,
                    init,
                    default_of,
                } => {
                    let mut_of = |id: &usize| if muts.contains(id) { "mut " } else { "" };
                    let ty = match vars.as_slice() {
                        [Some(id)] => self.vars.get(*id).and_then(|v| v.ty.clone()),
                        _ => None,
                    };
                    let annot = ty.map(|t| format!(": {t}")).unwrap_or_default();
                    if let ([Some(id)], Some(init)) = (vars.as_slice(), init) {
                        // A literal returned right away is the tail.
                        let rest = stmts.get(i + 1..).unwrap_or_default();
                        if default_of.is_some()
                            && ending == Ending::EndsBody
                            && matches!(rest, [S::Return(Some(e))] if e.code == self.name(*id))
                        {
                            put!(out, "{}", self.tail(&init.code, mode));
                            return;
                        }
                    }
                    let names: Vec<String> = vars
                        .iter()
                        .map(|v| {
                            v.map_or_else(
                                || "_".to_owned(),
                                |id| format!("{}{}", mut_of(&id), self.name(id)),
                            )
                        })
                        .collect();
                    let pat = if names.len() == 1 {
                        names.concat()
                    } else {
                        format!("({})", names.join(", "))
                    };
                    if let Some(init) = init {
                        put!(out, "let {pat}{annot} = {};\n", init.code);
                    } else {
                        if let [Some(id)] = vars.as_slice() {
                            let next = stmts
                                .iter()
                                .enumerate()
                                .skip(i + 1)
                                .find(|(_, s)| mentions(s, *id));
                            if let Some((j, next)) = next
                                && let Some(value) = self.late_init(*id, next)
                            {
                                let text = format!(
                                    "let {}{}{annot} = {value};\n",
                                    mut_of(id),
                                    self.name(*id)
                                );
                                merged.insert(j, text);
                                i += 1;
                                continue;
                            }
                        }
                        put!(out, "let {pat}{annot};\n");
                    }
                }
                S::Set { var, rhs } => {
                    put!(out, "{} = {};\n", self.name(*var), rhs.code);
                }
                S::Store { place, rhs, .. } => {
                    put!(out, "{place} = {};\n", rhs.code);
                }
                S::Expr(e) => {
                    let code = e.code.trim_end();
                    if code.ends_with('}') && (code.starts_with("for ") || code.starts_with("if "))
                    {
                        put!(out, "{code}\n");
                    } else {
                        put!(out, "{};\n", code.trim_start_matches('*'));
                    }
                }
                S::If { cond, then, els } => {
                    self.print_if(cond, then, els, muts, out, mode, statement_ending);
                    out.push('\n');
                }
                S::Match { scrut, arms } => {
                    self.print_match(scrut, arms, muts, out, mode, statement_ending);
                    out.push('\n');
                }
                S::Return(e) => {
                    let value = e.as_ref().map_or("()", |e| e.code.as_str());
                    if is_last && mode != PrintMode::Loop {
                        put!(out, "{}", self.tail(value, mode));
                    } else {
                        put!(out, "return {};\n", self.fn_value(value));
                    }
                }
                S::Fail(e) => {
                    put!(out, "return Err({});\n", e.code);
                }
            }
            i += 1;
        }
    }

    /// The tail expression giving `value`.
    fn tail(&self, value: &str, mode: PrintMode) -> String {
        match mode {
            PrintMode::Closure => format!("Ok({value})"),
            PrintMode::Block => value.to_owned(),
            PrintMode::Fn | PrintMode::Loop => {
                let v = self.fn_value(value);
                if v == "()" { String::new() } else { v }
            }
        }
    }

    /// `value` as returned from the function.
    fn fn_value(&self, value: &str) -> String {
        if self.fallible {
            format!("Ok({value})")
        } else {
            value.to_owned()
        }
    }

    /// The value of a late-initialized local when `next` assigns it on every path.
    fn late_init(&self, id: usize, next: &S) -> Option<String> {
        match next {
            S::Set { var, rhs } if *var == id && !rhs.reads.contains(&id) => Some(rhs.code.clone()),
            S::Match { scrut, arms } if arms.iter().any(|(p, _)| p.is_empty()) => {
                let mut parts = Vec::new();
                for (pats, body) in arms {
                    let value = self.branch_value(id, body)?;
                    let pat = if pats.is_empty() {
                        "_".to_owned()
                    } else {
                        compress(pats)
                    };
                    parts.push(format!("{pat} => {value},"));
                }
                Some(format!("match {} {{ {} }}", scrut.code, parts.join(" ")))
            }
            S::If {
                cond: Cond::Plain { e, .. },
                then,
                els,
            } if !els.is_empty() => {
                let a = self.branch_value(id, then)?;
                let b = self.branch_value(id, els)?;
                Some(format!("if {} {{ {a} }} else {{ {b} }}", e.code))
            }
            _ => None,
        }
    }

    /// The statements of a branch followed by the value it assigns to `id` last.
    fn branch_value(&self, id: usize, body: &[S]) -> Option<String> {
        let (last, init) = body.split_last()?;
        let S::Set { var, rhs } = last else {
            return None;
        };
        if *var != id || init.iter().any(|s| mentions(s, id)) {
            return None;
        }
        if init.is_empty() {
            return Some(rhs.code.clone());
        }
        let mut out = String::new();
        self.print(
            init,
            &Vars::new(),
            &mut out,
            PrintMode::Loop,
            Ending::Inside,
        );
        Some(format!("{{ {out} {} }}", rhs.code))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "consistency: printing passes one context to every statement kind"
    )]
    fn print_if(
        &self,
        cond: &Cond,
        then: &[S],
        els: &[S],
        muts: &Vars,
        out: &mut String,
        mode: PrintMode,
        ending: Ending,
    ) {
        let (cond_code, then, els) = match cond {
            Cond::Plain { e, neg } => {
                let negated = e.code.starts_with('!') || e.code.contains(" != ");
                if then.is_empty() && !els.is_empty() {
                    (
                        neg.clone().unwrap_or_else(|| format!("!({})", e.code)),
                        els,
                        then,
                    )
                } else if !els.is_empty() && negated && neg.is_some() {
                    (neg.clone().unwrap_or_default(), els, then)
                } else {
                    (e.code.clone(), then, els)
                }
            }
            Cond::Let { var, expr, extra } => {
                let used = then.iter().any(|s| mentions(s, *var))
                    || extra.as_ref().is_some_and(|x| x.reads.contains(var));
                let name = if used {
                    let m = if muts.contains(var) { "mut " } else { "" };
                    format!("{m}{}", self.name(*var))
                } else {
                    "_".to_owned()
                };
                let mut c = format!("let Some({name}) = {}", expr.code);
                if let Some(x) = extra {
                    put!(c, " && {}", x.code);
                }
                (c, then, els)
            }
        };
        // `if a { if b { .. } }` is `if a && b { .. }` (clippy::collapsible_if).
        if els.is_empty()
            && let [
                S::If {
                    cond: Cond::Plain { e, .. },
                    then: inner,
                    els: inner_els,
                },
            ] = then
            && inner_els.is_empty()
            && !inner.is_empty()
        {
            put!(
                out,
                "if {} && {} {{ ",
                paren_or(&cond_code),
                paren_or(&e.code)
            );
            self.print(inner, muts, out, mode, ending);
            out.push('}');
            return;
        }
        put!(out, "if {cond_code} {{ ");
        self.print(then, muts, out, mode, ending);
        out.push('}');
        if els.is_empty() {
            return;
        }
        out.push_str(" else ");
        if let [S::If { cond, then, els }] = els {
            self.print_if(cond, then, els, muts, out, mode, ending);
            return;
        }
        out.push_str("{ ");
        self.print(els, muts, out, mode, ending);
        out.push('}');
    }

    fn print_match(
        &self,
        scrut: &E,
        arms: &[(Vec<String>, Vec<S>)],
        muts: &Vars,
        out: &mut String,
        mode: PrintMode,
        ending: Ending,
    ) {
        // Arms with the same body merge (clippy::match_same_arms); without a default, empty
        // arms are the implicit `_ => {}`.
        let mut printed: Vec<(Vec<String>, String)> = Vec::new();
        let mut default: Option<String> = None;
        for (pats, body) in arms {
            let mut b = String::new();
            self.print(body, muts, &mut b, mode, ending);
            if pats.is_empty() {
                default = Some(b);
                continue;
            }
            if let Some(entry) = printed.iter_mut().find(|(_, x)| *x == b) {
                entry.0.extend(pats.iter().cloned());
                continue;
            }
            printed.push((pats.clone(), b));
        }
        let default = default.unwrap_or_default();
        printed.retain(|(_, b)| *b != default);
        match printed.as_slice() {
            [] => {
                if !default.trim().is_empty() {
                    put!(out, "{{ {default} }}");
                }
            }
            [(pats, body)] => {
                let cond = pats
                    .iter()
                    .map(|p| format!("{} == {p}", scrut.code))
                    .collect::<Vec<_>>()
                    .join(" || ");
                put!(out, "if {cond} {{ {body} }}");
                if !default.trim().is_empty() {
                    put!(out, " else {{ {default} }}");
                }
            }
            _ => {
                put!(out, "match {} {{ ", scrut.code);
                for (pats, body) in &printed {
                    put!(out, "{} => {{ {body} }} ", compress(pats));
                }
                put!(out, "_ => {{ {default} }} }}");
            }
        }
    }
}

/// Lowers a function in one direction (pure functions use `Dir::Encode`: borrowed values).
#[expect(
    clippy::too_many_lines,
    reason = "consistency: parameters, body and signature in one place"
)]
pub(crate) fn function(
    m: &Model<'_>,
    sums: &Summaries,
    dir: Dir,
    f: &Function,
    sum: &Summary,
) -> Result<Lowered, String> {
    let module = model::module_of(&f.name);
    let mut l = Lower::new(m, sums, dir, module);
    let all: Vec<&Param> = f.receiver.iter().chain(&f.params).collect();
    let mut params = Vec::new();
    let mut in_out = Vec::new();
    let mut init = Vars::new();
    for (i, p) in all.iter().enumerate() {
        if !sum.used.get(i).copied().unwrap_or(true) {
            continue;
        }
        let copy = m.is_copy(&p.ty);
        let name = param_name(&p.name)?;
        if p.by_ref && !(dir == Dir::Encode && copy) {
            let rust = l.rust_type(&p.ty)?;
            let (code, kind, ty) = match dir {
                Dir::Decode => (format!("*{name}"), BKind::RefMut, format!("&mut {rust}")),
                Dir::Encode => (name.clone(), BKind::Ref, format!("&{rust}")),
            };
            params.push((None, format!("{name}: {ty}")));
            l.bind(
                &p.name,
                Binding {
                    code,
                    ty: p.ty.clone(),
                    kind,
                    var: None,
                    external: true,
                    borrowed: false,
                },
            );
        } else if !copy && !is_string(&p.ty) {
            let rust = l.rust_type(&p.ty)?;
            params.push((None, format!("{name}: &{rust}")));
            l.bind(
                &p.name,
                Binding {
                    code: name,
                    ty: p.ty.clone(),
                    kind: BKind::Ref,
                    var: None,
                    external: true,
                    borrowed: false,
                },
            );
        } else if is_string(&p.ty) {
            params.push((None, format!("{name}: &str")));
            l.bind(
                &p.name,
                Binding {
                    code: name,
                    ty: p.ty.clone(),
                    kind: BKind::Ref,
                    var: None,
                    external: true,
                    borrowed: false,
                },
            );
        } else {
            let id = l.new_var(&p.name, None)?;
            if sum.in_out.contains(&i) {
                in_out.push(id);
            }
            init.insert(id);
            params.push((Some(id), format!(": {}", l.rust_type(&p.ty)?)));
            l.bind(
                &p.name,
                Binding {
                    code: l.name(id),
                    ty: p.ty.clone(),
                    kind: BKind::Owned,
                    var: Some(id),
                    external: false,
                    borrowed: false,
                },
            );
        }
    }
    let results = f
        .results
        .iter()
        .map(|t| l.result_type(t, &sum.result))
        .collect::<Result<Vec<_>, String>>()?;
    l.ret = Ret::Values { results, in_out };
    let mut out = Vec::new();
    if dir == Dir::Encode {
        l.add_shadows(&f.body, &mut out)?;
    }
    l.stmts(&f.body, &mut out)?;
    if !ends(&out) {
        out.push(l.return_stmt(&[])?);
    }
    live(&mut out, &Vars::new());
    let (body, muts) = l.print_block(&out, PrintMode::Fn, &mut init);
    let params = params
        .into_iter()
        .map(|(id, text)| match id {
            Some(id) => {
                let m = if muts.contains(&id) { "mut " } else { "" };
                format!("{m}{}{text}", l.name(id))
            }
            None => text,
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Lowered {
        params,
        body,
        fallible: l.fallible,
        helpers: l.helpers,
    })
}

/// Lowers a `Marshal` body of the Go type `go` in one direction.
pub(crate) fn marshal(
    m: &Model<'_>,
    sums: &Summaries,
    dir: Dir,
    module: Module,
    go: &str,
    receiver: &str,
    stmts: &[Stmt],
) -> Result<Lowered, String> {
    let mut l = Lower::new(m, sums, dir, module);
    let t = named_ty(go);
    // An unnamed receiver (`func (*T) Marshal(IO)`) is never referenced.
    let receiver = if receiver.is_empty() || receiver == "_" {
        "x"
    } else {
        receiver
    };
    // `Codec` methods always return `Result`.
    l.fallible = true;
    let mut out = Vec::new();
    let mut init = Vars::new();
    match dir {
        Dir::Decode => {
            let id = l.new_var(receiver, None)?;
            let fields = m.struct_def(go).map_or(usize::MAX, |d| d.fields.len());
            l.bind(
                receiver,
                Binding {
                    code: l.name(id),
                    ty: t,
                    kind: BKind::Owned,
                    var: Some(id),
                    external: false,
                    borrowed: false,
                },
            );
            out.push(S::Let {
                vars: vec![Some(id)],
                init: Some(E::new("Self::default()".to_owned())),
                default_of: Some(("Self".to_owned(), fields)),
            });
            l.ret = Ret::Receiver(id);
        }
        Dir::Encode => {
            l.bind(
                receiver,
                Binding {
                    code: "self".to_owned(),
                    ty: t,
                    kind: BKind::Ref,
                    var: None,
                    external: true,
                    borrowed: false,
                },
            );
            l.add_shadows(stmts, &mut out)?;
        }
    }
    l.stmts(stmts, &mut out)?;
    if !ends(&out) {
        out.push(l.return_stmt(&[])?);
    }
    live(&mut out, &Vars::new());
    let (body, _) = l.print_block(&out, PrintMode::Fn, &mut init);
    Ok(Lowered {
        params: String::new(),
        body,
        fallible: l.fallible,
        helpers: l.helpers,
    })
}

fn ends(out: &[S]) -> bool {
    matches!(out.last(), Some(S::Return(_) | S::Fail(_)))
}

fn named_ty(go: &str) -> Type {
    Type {
        kind: "named".to_owned(),
        name: go.to_owned(),
        len: None,
        key: None,
        elem: None,
        elems: Vec::new(),
    }
}

fn basic_ty(name: &str) -> Type {
    Type {
        kind: "basic".to_owned(),
        name: name.to_owned(),
        len: None,
        key: None,
        elem: None,
        elems: Vec::new(),
    }
}

fn is_int(prim: Option<&str>) -> bool {
    prim.is_some_and(|p| p.starts_with('u') || p.starts_with('i'))
}

/// The bit width of a Rust integer type; `usize` counts as 64.
pub(crate) fn bits(prim: &str) -> u32 {
    match prim {
        "u8" | "i8" => 8,
        "u16" | "i16" => 16,
        "u32" | "i32" => 32,
        _ => 64,
    }
}

/// A Rust integer literal from a decimal string, digits grouped by three from five digits on.
pub(crate) fn int_lit(value: &str) -> String {
    let (sign, digits) = value.strip_prefix('-').map_or(("", value), |d| ("-", d));
    if digits.len() < 5 {
        return value.to_owned();
    }
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('_');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// A Rust float literal from a Go `g` formatted value.
fn float_lit(value: &str) -> Result<String, String> {
    let f: f64 = value
        .parse()
        .map_err(|_| format!("bad float constant {value}"))?;
    let text = format!("{f:?}");
    if text.contains('e') || text.contains("inf") || text.contains("NaN") {
        return Err(format!("unsupported float constant {value}"));
    }
    let (int, frac) = text.split_once('.').unwrap_or((&text, "0"));
    Ok(format!("{}.{frac}", int_lit(int)))
}

/// A Rust string literal.
pub(crate) fn str_lit(value: &str) -> String {
    format!("{value:?}")
}

/// Folds the field stores that follow a default struct into its literal
/// (`clippy::field_reassign_with_default`).
fn fold(stmts: &[S]) -> Vec<S> {
    let mut out = Vec::with_capacity(stmts.len());
    let mut i = 0;
    while let Some(s) = stmts.get(i) {
        if let S::Let {
            vars,
            init: Some(init),
            default_of: Some((t, fields)),
        } = s
            && let [Some(id)] = vars.as_slice()
        {
            let rest = stmts.get(i + 1..).unwrap_or_default();
            let (lit, used) = fold_literal(t, *fields, *id, rest);
            let mut e = init.clone();
            let mut folded = 0;
            let mut declarations = Vec::new();
            for st in rest {
                if folded == used {
                    break;
                }
                match st {
                    S::Store { rhs, .. } => {
                        e = e.with(rhs);
                        folded += 1;
                    }
                    other => declarations.push(other.clone()),
                }
                i += 1;
            }
            e.code = lit;
            out.push(S::Let {
                vars: vars.clone(),
                init: Some(e),
                default_of: Some((t.clone(), *fields)),
            });
            out.extend(declarations);
            i += 1;
            continue;
        }
        out.push(s.clone());
        i += 1;
    }
    out
}

/// The struct literal of a default value with the field stores after it folded in, and how many
/// statements were folded.
fn fold_literal(t: &str, total: usize, id: usize, rest: &[S]) -> (String, usize) {
    let mut fields: Vec<(String, String)> = Vec::new();
    for s in rest {
        if matches!(s, S::Let { init: None, .. }) {
            // A declaration without a value does not use the struct.
            continue;
        }
        let S::Store {
            root: Some(r),
            field: Some(f),
            rhs,
            ..
        } = s
        else {
            break;
        };
        if *r != id || rhs.reads.contains(&id) || fields.iter().any(|(n, _)| n == f) {
            break;
        }
        fields.push((f.clone(), rhs.code.clone()));
    }
    let used = fields.len();
    if used == 0 {
        return (format!("{t}::default()"), 0);
    }
    let mut parts: Vec<String> = fields
        .into_iter()
        .map(|(n, v)| if n == v { n } else { format!("{n}: {v}") })
        .collect();
    if used < total {
        parts.push(format!("..{t}::default()"));
    }
    (format!("{t} {{ {} }}", parts.join(", ")), used)
}

/// Removes dead stores to locals, backwards from the locals live after the list; returns the
/// locals live before it.
#[expect(
    clippy::too_many_lines,
    reason = "consistency: one match arm per statement kind"
)]
fn live(stmts: &mut Vec<S>, out_live: &Vars) -> Vars {
    let mut l = out_live.clone();
    // Processed statements in reverse order.
    let mut done: Vec<S> = Vec::new();
    let old = std::mem::take(stmts);
    for s in old.into_iter().rev() {
        let later_mentions = |id: usize, done: &[S]| done.iter().any(|s| mentions(s, id));
        match s {
            S::Let {
                mut vars,
                init,
                default_of,
            } => {
                let declared: Vec<usize> = vars.iter().flatten().copied().collect();
                let any_live = declared.iter().any(|d| l.contains(d));
                let mut deferred = Vec::new();
                if any_live {
                    // Components dead here but assigned later get their own declaration.
                    for v in &mut vars {
                        if let Some(id) = *v
                            && !l.contains(&id)
                        {
                            *v = None;
                            if later_mentions(id, &done) {
                                deferred.push(id);
                            }
                        }
                    }
                    for d in &declared {
                        l.remove(d);
                    }
                    for id in deferred {
                        done.push(S::Let {
                            vars: vec![Some(id)],
                            init: None,
                            default_of: None,
                        });
                    }
                    if let Some(e) = &init {
                        l.extend(&e.reads);
                    }
                    done.push(S::Let {
                        vars,
                        init,
                        default_of,
                    });
                    continue;
                }
                for d in &declared {
                    l.remove(d);
                }
                let used: Vec<usize> = declared
                    .iter()
                    .copied()
                    .filter(|d| later_mentions(*d, &done))
                    .collect();
                for id in &used {
                    done.push(S::Let {
                        vars: vec![Some(*id)],
                        init: None,
                        default_of: None,
                    });
                }
                if let Some(e) = init
                    && e.effects
                {
                    l.extend(&e.reads);
                    done.push(S::Expr(effect_only(e)));
                }
            }
            S::Set { var, rhs } => {
                if l.contains(&var) {
                    l.remove(&var);
                    l.extend(&rhs.reads);
                    done.push(S::Set { var, rhs });
                } else if rhs.effects {
                    l.extend(&rhs.reads);
                    done.push(S::Expr(effect_only(rhs)));
                }
            }
            S::Store {
                place,
                root,
                field,
                reads,
                rhs,
            } => {
                l.extend(reads.iter());
                l.extend(&rhs.reads);
                done.push(S::Store {
                    place,
                    root,
                    field,
                    reads,
                    rhs,
                });
            }
            S::Expr(e) => {
                l.extend(&e.reads);
                l.extend(&e.muts);
                done.push(S::Expr(e));
            }
            S::If {
                cond,
                mut then,
                mut els,
            } => {
                let a = live(&mut then, &l);
                let b = live(&mut els, &l);
                l = a.union(&b).copied().collect();
                let pure = match &cond {
                    Cond::Plain { e, .. } => {
                        l.extend(&e.reads);
                        !e.effects
                    }
                    Cond::Let { var, expr, extra } => {
                        l.remove(var);
                        if let Some(x) = extra {
                            l.extend(&x.reads);
                        }
                        l.remove(var);
                        l.extend(&expr.reads);
                        !expr.effects && extra.as_ref().is_none_or(|x| !x.effects)
                    }
                };
                if then.is_empty() && els.is_empty() && pure {
                    continue;
                }
                done.push(S::If { cond, then, els });
            }
            S::Match { scrut, mut arms } => {
                let mut acc = Vars::new();
                if !arms.iter().any(|(p, _)| p.is_empty()) {
                    acc.extend(&l);
                }
                for (_, body) in &mut arms {
                    acc.extend(live(body, &l));
                }
                l = acc;
                l.extend(&scrut.reads);
                if arms.iter().all(|(_, b)| b.is_empty()) && !scrut.effects {
                    continue;
                }
                done.push(S::Match { scrut, arms });
            }
            S::Return(e) => {
                l = e.as_ref().map(|e| e.reads.clone()).unwrap_or_default();
                done.push(S::Return(e));
            }
            S::Fail(e) => {
                l.clone_from(&e.reads);
                done.push(S::Fail(e));
            }
        }
    }
    done.reverse();
    *stmts = done;
    l
}

/// An expression evaluated only for its effect: conversions of the value are dropped.
fn effect_only(mut e: E) -> E {
    let mut code = e.code.trim().to_owned();
    loop {
        let before = code.clone();
        for suffix in [".to_owned()", ".clone()", ".as_str()"] {
            if code.ends_with(suffix) {
                code.truncate(code.len() - suffix.len());
            }
        }
        if code.starts_with("(*") && code.ends_with(')') {
            code.truncate(code.len() - 1);
            code.drain(..2);
        }
        if code.starts_with('*') {
            code.remove(0);
        }
        if code == before {
            break;
        }
    }
    e.code = code;
    e
}

/// Whether a statement mentions a local.
fn mentions(s: &S, id: usize) -> bool {
    match s {
        S::Let { vars, init, .. } => {
            vars.contains(&Some(id)) || init.as_ref().is_some_and(|e| e.reads.contains(&id))
        }
        S::Set { var, rhs } => *var == id || rhs.reads.contains(&id),
        S::Store {
            root, reads, rhs, ..
        } => *root == Some(id) || reads.contains(&id) || rhs.reads.contains(&id),
        S::Expr(e) => e.reads.contains(&id) || e.muts.contains(&id),
        S::If { cond, then, els } => {
            let c = match cond {
                Cond::Plain { e, .. } => e.reads.contains(&id),
                Cond::Let { var, expr, extra } => {
                    *var == id
                        || expr.reads.contains(&id)
                        || extra.as_ref().is_some_and(|x| x.reads.contains(&id))
                }
            };
            c || then.iter().any(|s| mentions(s, id)) || els.iter().any(|s| mentions(s, id))
        }
        S::Match { scrut, arms } => {
            scrut.reads.contains(&id) || arms.iter().any(|(_, b)| b.iter().any(|s| mentions(s, id)))
        }
        S::Return(e) => e.as_ref().is_some_and(|e| e.reads.contains(&id)),
        S::Fail(e) => e.reads.contains(&id),
    }
}

/// Forward pass: locals assigned when they may already hold a value need `mut`.
fn assigned(stmts: &[S], state: &mut Option<Vars>, muts: &mut Vars) {
    for s in stmts {
        let Some(st) = state.as_mut() else { return };
        match s {
            S::Let { vars, init, .. } => {
                if init.is_some() {
                    st.extend(vars.iter().flatten());
                }
            }
            S::Set { var, .. } => {
                if st.contains(var) {
                    muts.insert(*var);
                }
                st.insert(*var);
            }
            S::If { then, els, .. } => {
                let mut a = state.clone();
                assigned(then, &mut a, muts);
                let mut b = state.clone();
                assigned(els, &mut b, muts);
                *state = union(a, b);
            }
            S::Match { arms, .. } => {
                let has_default = arms.iter().any(|(p, _)| p.is_empty());
                let mut acc = if has_default { None } else { state.clone() };
                for (_, body) in arms {
                    let mut a = state.clone();
                    assigned(body, &mut a, muts);
                    acc = union(acc, a);
                }
                *state = acc;
            }
            S::Return(_) | S::Fail(_) => *state = None,
            S::Store { .. } | S::Expr(_) => {}
        }
    }
}

fn union(a: Option<Vars>, b: Option<Vars>) -> Option<Vars> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(&b).copied().collect()),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Locals written through a field or borrowed mutably need `mut`.
fn collect_muts(stmts: &[S], muts: &mut Vars) {
    for s in stmts {
        match s {
            S::Store {
                root, place, rhs, ..
            } => {
                if !place.starts_with('*') {
                    muts.extend(root.iter());
                }
                muts.extend(&rhs.muts);
            }
            S::Expr(e) | S::Set { rhs: e, .. } | S::Let { init: Some(e), .. } => {
                muts.extend(&e.muts);
            }
            S::If { then, els, cond } => {
                if let Cond::Plain { e, .. } = cond {
                    muts.extend(&e.muts);
                }
                collect_muts(then, muts);
                collect_muts(els, muts);
            }
            S::Match { arms, .. } => {
                for (_, b) in arms {
                    collect_muts(b, muts);
                }
            }
            S::Let { init: None, .. } | S::Return(_) | S::Fail(_) => {}
        }
    }
}

/// Whether statements assign a local declared before `first`.
fn sets_before(stmts: &[S], first: usize) -> bool {
    stmts.iter().any(|s| match s {
        S::Set { var, .. } => *var < first,
        S::If { then, els, .. } => sets_before(then, first) || sets_before(els, first),
        S::Match { arms, .. } => arms.iter().any(|(_, b)| sets_before(b, first)),
        _ => false,
    })
}

/// The locals declared before `first` that statements read.
fn outer(stmts: &[S], first: usize) -> Vars {
    fn walk(stmts: &[S], all: &mut Vars) {
        for s in stmts {
            match s {
                S::Let { init, .. } => all.extend(init.iter().flat_map(|e| e.reads.iter())),
                S::Set { rhs: e, .. } | S::Expr(e) | S::Fail(e) => all.extend(&e.reads),
                S::Store {
                    reads, rhs, root, ..
                } => {
                    all.extend(reads.iter());
                    all.extend(&rhs.reads);
                    all.extend(root.iter());
                }
                S::If { cond, then, els } => {
                    match cond {
                        Cond::Plain { e, .. } => all.extend(&e.reads),
                        Cond::Let { expr, extra, .. } => {
                            all.extend(&expr.reads);
                            all.extend(extra.iter().flat_map(|x| x.reads.iter()));
                        }
                    }
                    walk(then, all);
                    walk(els, all);
                }
                S::Match { scrut, arms } => {
                    all.extend(&scrut.reads);
                    for (_, b) in arms {
                        walk(b, all);
                    }
                }
                S::Return(e) => all.extend(e.iter().flat_map(|e| e.reads.iter())),
            }
        }
    }
    let mut all = Vars::new();
    walk(stmts, &mut all);
    all.into_iter().filter(|v| *v < first).collect()
}

/// The Go path (`x.A.B`) of a place made of a variable and fields.
fn place_path(e: &Expr) -> Option<String> {
    match e.kind.as_str() {
        "var" => Some(e.name.clone()),
        "field" => Some(format!("{}.{}", place_path(e.x.as_deref()?)?, e.name)),
        _ => None,
    }
}

fn root_name(e: &Expr) -> Option<String> {
    match e.kind.as_str() {
        "var" => Some(e.name.clone()),
        "field" | "index" | "deref" => root_name(e.x.as_deref()?),
        _ => None,
    }
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || b.starts_with(&format!("{a}.")) || a.starts_with(&format!("{b}."))
}

/// Records the place paths an expression reads.
fn expr_paths(e: &Expr, out: &mut Vec<(String, bool)>) {
    if let Some(p) = place_path(e) {
        out.push((p, false));
        return;
    }
    for x in [&e.x, &e.y, &e.index, &e.low, &e.high, &e.recv]
        .into_iter()
        .flatten()
    {
        expr_paths(x, out);
    }
    for a in e.args.iter().chain(&e.elems) {
        expr_paths(a, out);
    }
    for f in &e.fields {
        expr_paths(&f.value, out);
    }
}

/// The Go names a body declares (locals, elements, loop variables).
fn collect_locals(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        if s.kind == "var" {
            out.insert(s.name.clone());
        }
        if s.kind == "assign" && s.op == ":=" || s.kind == "range" {
            for e in s.lhs.iter().chain(&s.key).chain(&s.range_value) {
                if e.kind == "var" {
                    out.insert(e.name.clone());
                }
            }
        }
        if let Some(e) = &s.elem {
            out.insert(e.name.clone());
        }
        for block in s.blocks() {
            collect_locals(block, out);
        }
    }
}

fn contains_return(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| {
        s.kind == "return"
            || contains_return(&s.then)
            || contains_return(&s.els)
            || contains_return(&s.body)
            || s.cases.iter().any(|c| contains_return(&c.body))
    })
}

/// Calls `f` on every expression with its role in its parent.
fn visit_exprs(stmts: &[Stmt], f: &mut dyn FnMut(&Expr, Role)) {
    fn walk(e: &Expr, role: Role, f: &mut dyn FnMut(&Expr, Role)) {
        f(e, role);
        for x in [&e.x, &e.y, &e.low, &e.high, &e.recv].into_iter().flatten() {
            walk(x, Role::Other, f);
        }
        if let Some(i) = &e.index {
            walk(i, Role::Index, f);
        }
        for a in e.args.iter().chain(&e.elems) {
            walk(a, Role::Other, f);
        }
        for fv in &e.fields {
            walk(&fv.value, Role::Other, f);
        }
    }
    for s in stmts {
        let singles = [
            &s.place,
            &s.len,
            &s.recv,
            &s.value,
            &s.cond,
            &s.tag,
            &s.x,
            &s.key,
            &s.range_value,
            &s.field,
            &s.reason,
            &s.enum_name,
        ];
        for e in singles.into_iter().flatten() {
            walk(e, Role::Other, f);
        }
        for e in s.args.iter().chain(&s.lhs).chain(&s.rhs).chain(&s.values) {
            walk(e, Role::Other, f);
        }
        for list in [&s.body, &s.then, &s.els, &s.init, &s.post] {
            visit_exprs(list, f);
        }
        for c in &s.cases {
            for v in &c.values {
                walk(v, Role::Other, f);
            }
            visit_exprs(&c.body, f);
        }
    }
}

/// Whether evaluating a Go expression may fail: it calls a function or indexes.
fn may_fail(e: &Expr) -> bool {
    matches!(e.kind.as_str(), "index" | "slice_of")
        || (e.kind == "call" && e.target != "builtin")
        || [&e.x, &e.y, &e.index, &e.low, &e.high, &e.recv]
            .into_iter()
            .flatten()
            .any(|x| may_fail(x))
        || e.args.iter().chain(&e.elems).any(may_fail)
        || e.fields.iter().any(|f| may_fail(&f.value))
}

/// Whether a Go variable is referenced in statements.
pub(crate) fn uses_var(stmts: &[Stmt], name: &str) -> bool {
    let mut used = false;
    visit_exprs(stmts, &mut |e, _| used |= e.kind == "var" && e.name == name);
    used
}

/// The single expression a loop variable indexes in a body, if it is used only as an index.
fn index_base(stmts: &[Stmt], var: &str) -> Option<Expr> {
    let mut base: Option<Expr> = None;
    let mut ok = true;
    visit_exprs(stmts, &mut |e, role| {
        if e.kind == "index"
            && e.index
                .as_deref()
                .is_some_and(|i| i.kind == "var" && i.name == var)
        {
            match (&base, e.x.as_deref()) {
                (None, Some(b)) => base = Some(b.clone()),
                (Some(x), Some(b)) if same_expr(x, b) => {}
                _ => ok = false,
            }
        } else if e.kind == "var" && e.name == var && role != Role::Index {
            ok = false;
        }
    });
    if ok { base } else { None }
}

fn same_expr(a: &Expr, b: &Expr) -> bool {
    let same = |x: &Option<Box<Expr>>, y: &Option<Box<Expr>>| match (x, y) {
        (Some(x), Some(y)) => same_expr(x, y),
        (None, None) => true,
        _ => false,
    };
    a.kind == b.kind
        && a.name == b.name
        && a.value == b.value
        && same(&a.x, &b.x)
        && same(&a.index, &b.index)
}

/// Whether a body (decoding) writes `base[var]` or a place inside it.
fn writes_index(stmts: &[Stmt], var: &str) -> bool {
    let indexed = |e: &Expr| {
        let mut cur = Some(e);
        while let Some(c) = cur {
            if c.kind == "index"
                && c.index
                    .as_deref()
                    .is_some_and(|i| i.kind == "var" && i.name == var)
            {
                return true;
            }
            cur = c.x.as_deref();
        }
        false
    };
    stmts.iter().any(|s| {
        let place_write = matches!(
            s.kind.as_str(),
            "io" | "marshal" | "slice" | "optional" | "integer_func"
        ) && s.place.as_ref().is_some_and(indexed);
        place_write
            || (s.kind == "assign" && s.lhs.iter().any(indexed))
            || (s.kind == "call" && s.args.iter().any(indexed))
            || writes_index(&s.body, var)
            || writes_index(&s.then, var)
            || writes_index(&s.els, var)
            || s.cases.iter().any(|c| writes_index(&c.body, var))
    })
}

/// Replaces `base[var]` with the element binding `#elem:var`.
fn replace_index(stmts: &[Stmt], var: &str, base: &Expr) -> Vec<Stmt> {
    fn fix(e: &mut Expr, var: &str, base: &Expr) {
        let hit = e.kind == "index"
            && e.index
                .as_deref()
                .is_some_and(|i| i.kind == "var" && i.name == var)
            && e.x.as_deref().is_some_and(|x| same_expr(x, base));
        if hit {
            "var".clone_into(&mut e.kind);
            e.name = format!("#elem:{var}");
            e.x = None;
            e.index = None;
            return;
        }
        for x in [
            &mut e.x,
            &mut e.y,
            &mut e.index,
            &mut e.low,
            &mut e.high,
            &mut e.recv,
        ]
        .into_iter()
        .flatten()
        {
            fix(x, var, base);
        }
        for a in e.args.iter_mut().chain(e.elems.iter_mut()) {
            fix(a, var, base);
        }
        for fv in &mut e.fields {
            fix(&mut fv.value, var, base);
        }
    }
    fn fix_stmts(stmts: &mut [Stmt], var: &str, base: &Expr) {
        for s in stmts {
            let singles = [
                &mut s.place,
                &mut s.len,
                &mut s.recv,
                &mut s.value,
                &mut s.cond,
                &mut s.tag,
                &mut s.x,
                &mut s.key,
                &mut s.range_value,
                &mut s.field,
                &mut s.reason,
                &mut s.enum_name,
            ];
            for e in singles.into_iter().flatten() {
                fix(e, var, base);
            }
            for e in s
                .args
                .iter_mut()
                .chain(s.lhs.iter_mut())
                .chain(s.rhs.iter_mut())
                .chain(s.values.iter_mut())
            {
                fix(e, var, base);
            }
            for list in [
                &mut s.body,
                &mut s.then,
                &mut s.els,
                &mut s.init,
                &mut s.post,
            ] {
                fix_stmts(list, var, base);
            }
            for c in &mut s.cases {
                fix_stmts(&mut c.body, var, base);
            }
        }
    }
    let mut out = stmts.to_vec();
    fix_stmts(&mut out, var, base);
    out
}

pub(crate) fn is_string(t: &Type) -> bool {
    t.kind == "basic" && (t.name == "string" || t.name == "untyped string")
}

fn prim_of(t: &Type) -> Option<&'static str> {
    if t.kind != "basic" || is_string(t) {
        return None;
    }
    model::basic(&t.name).ok()
}

fn paren(code: &str) -> String {
    let simple = !code.starts_with('-')
        && code.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '(' | ')' | ':' | '[' | ']' | '?')
        });
    if simple {
        code.to_owned()
    } else {
        format!("({code})")
    }
}

/// A method receiver: a dereference needs parentheses.
fn method_recv(code: &str) -> String {
    if code.starts_with('*') {
        format!("({code})")
    } else {
        code.to_owned()
    }
}

fn paren_or(code: &str) -> String {
    if code.contains("||") {
        format!("({code})")
    } else {
        code.to_owned()
    }
}

/// A type usable as a path prefix (`<[f32; 3]>::default()`).
pub(crate) fn type_path(rust: &str) -> String {
    if rust.starts_with('[') || rust.contains('<') {
        format!("<{rust}>")
    } else {
        rust.to_owned()
    }
}

/// Whether printed code names a Rust identifier (not as a field).
fn mentions_name(code: &str, name: &str) -> bool {
    let bytes = code.as_bytes();
    let ident = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let mut start = 0;
    while let Some(pos) = code.get(start..).and_then(|c| c.find(name)) {
        let at = start + pos;
        let end = at + name.len();
        let before = at.checked_sub(1).and_then(|i| bytes.get(i));
        if !ident(before) && !ident(bytes.get(end)) && before != Some(&b'.') {
            return true;
        }
        start = end;
    }
    false
}

/// An or-pattern with runs of three or more consecutive integers as ranges
/// (`clippy::manual_range_patterns`).
pub(crate) fn compress(pats: &[String]) -> String {
    let mut nums: Vec<i64> = Vec::new();
    for p in pats {
        match p.replace('_', "").parse() {
            Ok(n) => nums.push(n),
            Err(_) => return pats.join(" | "),
        }
    }
    nums.sort_unstable();
    nums.dedup();
    let mut ranges = Vec::new();
    let mut i = 0;
    while let Some(&start) = nums.get(i) {
        let mut j = i;
        while nums
            .get(j + 1)
            .is_some_and(|&n| nums.get(j).is_some_and(|&p| p + 1 == n))
        {
            j += 1;
        }
        let end = nums.get(j).copied().unwrap_or(start);
        if j - i >= 2 {
            ranges.push(format!(
                "{}..={}",
                int_lit(&start.to_string()),
                int_lit(&end.to_string())
            ));
        } else {
            for n in nums.get(i..=j).unwrap_or_default() {
                ranges.push(int_lit(&n.to_string()));
            }
        }
        i = j + 1;
    }
    ranges.join(" | ")
}

/// The Rust name of a struct field.
pub(crate) fn field_rust_name(ty: &str, go: &str) -> Result<String, String> {
    names::snake(go, &format!("{ty}.{go}"))
}

/// The Rust name of a Go local or parameter (`r`, `w` and `self` are taken).
fn param_name(go: &str) -> Result<String, String> {
    let name = names::snake(go, go)?;
    Ok(if matches!(name.as_str(), "r" | "w" | "self" | "r#self") {
        format!("{}_", name.trim_start_matches("r#"))
    } else {
        name
    })
}

/// The Rust name of a generated function.
pub(crate) fn fn_name(go: &str, prefix: &str) -> Result<String, String> {
    let local = go.split_once('.').map_or(go, |(_, n)| n);
    let snake = local
        .split('.')
        .map(|part| names::snake(part, go).map(|s| s.trim_start_matches("r#").to_owned()))
        .collect::<Result<Vec<_>, String>>()?
        .join("_");
    Ok(format!("{prefix}{snake}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_literals_group_thousands_from_five_digits() {
        assert_eq!(int_lit("4096"), "4096");
        assert_eq!(int_lit("16384"), "16_384");
        assert_eq!(int_lit("-12345"), "-12_345");
        assert_eq!(int_lit("1234567"), "1_234_567");
    }
}
